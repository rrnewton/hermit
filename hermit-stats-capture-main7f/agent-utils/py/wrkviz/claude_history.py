"""Claude Code prompt-history importer: every prompt the owner typed, with or without its transcript.

Claude Code keeps two records of a session. The transcript under ``~/.claude/projects/`` is the
complete one -- prompts, responses, tool calls, subagents -- and :mod:`wrkviz.claude` reads it.
It is also the one Claude Code *deletes*: its ``cleanupPeriodDays`` setting, thirty days by
default, removes any transcript not modified inside that window, and it does so silently. The
other record is ``~/.claude/history.jsonl``: one line per prompt the owner submitted, carrying the
verbatim text, the pasted attachments, a millisecond timestamp, the working directory it was typed
in, and the session it belonged to. It is never cleaned up. On the archive that motivated this
module, the transcripts reached back nine weeks and the history reached back eleven months.

This importer turns the history into one normalized team so that those prompts take their place
on the timeline and in the prompt projection beside the fully-transcribed sessions. What it
produces is deliberately narrow and deliberately labelled:

* every record is a ``user_prompt`` event whose ``ingress_kind`` is ``claude_history``; nothing
  here is a response, a tool call, or an inter-agent message, because the history has none;
* each Claude session in the history is one ``coordinator`` agent whose lifetime is the span of
  its prompts, so the prompt projection -- which reads coordinator threads only -- sees all of
  them;
* a prompt's one-second turn is a placeholder in the same sense an Orc task note's is: it marks an
  instant, it does not measure a duration.

The history mixes every project the owner ever typed into, so a team is selected by a working
directory pattern the operator states explicitly. Nothing is inferred from the prompt prose. And a
session that *does* still have its transcript is normally registered as its own ``claude`` team,
whose prompts would otherwise appear twice; the operator names those sessions as *covered* and the
importer leaves them out, reporting how many it left. A project-config ingest derives that list
from the config's own ``claude`` teams, so the two never disagree.
"""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
from typing import Mapping, Sequence

from wrkviz.archive import write_text_if_changed
from wrkviz.claude import ClaudeSourceCopy
from wrkviz.model import Agent, Event, SourceSnapshot, TeamData, Turn


PROVIDER = "claude-history"
INGRESS_KIND = "claude_history"
_CLASSIFICATION_VERSION = "authorship-v1"
#: Prompts whose history line carries no ``sessionId`` -- the oldest Claude Code releases wrote
#: none -- are filed under this thread so they are still projected rather than dropped.
UNATTRIBUTED_SESSION = "unattributed"
#: A history prompt is an instant. The turn that carries it is one second long so that phase
#: construction has a non-empty interval to work with; it is not a measured duration.
_PLACEHOLDER_TURN_MS = 1_000
_SAFE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")


class ClaudeHistoryParseError(ValueError):
    """Raised when a history file, a source snapshot, or a selection is unusable."""


@dataclass(frozen=True)
class ClaudeHistorySnapshotResult:
    """Result of copying a complete history file into the snapshot store."""

    source: ClaudeSourceCopy
    files_changed: int


@dataclass(frozen=True)
class ClaudeHistorySelection:
    """What one history ingest kept and what it deliberately left out."""

    matched_prompts: int
    covered_prompts: int
    covered_sessions: int
    unmatched_prompts: int


@dataclass(frozen=True)
class _HistoryPrompt:
    line: int
    timestamp_ms: int
    session_id: str
    project: str
    text: str


def _complete_jsonl(data: bytes) -> bytes:
    end = data.rfind(b"\n")
    return b"" if end < 0 else data[: end + 1]


def _required_string(value: object, where: str) -> str:
    if not isinstance(value, str) or not value:
        raise ClaudeHistoryParseError(f"{where}: expected a non-empty string")
    return value


def compile_project_pattern(pattern: str) -> re.Pattern[str]:
    """Compile the operator's working-directory selector, refusing an empty or invalid one."""

    if not pattern:
        raise ClaudeHistoryParseError("project pattern must not be empty")
    try:
        return re.compile(pattern)
    except re.error as exc:
        raise ClaudeHistoryParseError(f"invalid project pattern {pattern!r}: {exc}") from exc


def _pasted_text(value: object, where: str) -> str:
    """Render ``pastedContents`` after the prompt, in attachment order, text attachments only."""

    if not isinstance(value, Mapping) or not value:
        return ""
    parts: list[str] = []
    for key in sorted(value, key=lambda item: (len(str(item)), str(item))):
        item = value[key]
        if not isinstance(item, Mapping):
            raise ClaudeHistoryParseError(f"{where}.pastedContents[{key!r}]: expected an object")
        if item.get("type") != "text":
            continue
        content = item.get("content")
        if not isinstance(content, str) or not content.strip():
            continue
        parts.append(f"[Pasted text #{key}]\n{content.rstrip()}")
    return "\n\n".join(parts)


def _parse_prompt(raw: bytes, line: int, where: str) -> _HistoryPrompt:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ClaudeHistoryParseError(f"{where}: invalid JSON: {exc}") from exc
    if not isinstance(value, Mapping):
        raise ClaudeHistoryParseError(f"{where}: expected a JSON object")
    display = _required_string(value.get("display"), where + ".display").strip()
    timestamp = value.get("timestamp")
    if isinstance(timestamp, bool) or not isinstance(timestamp, int) or timestamp < 0:
        raise ClaudeHistoryParseError(f"{where}.timestamp: expected a millisecond integer")
    project = _required_string(value.get("project"), where + ".project")
    session = value.get("sessionId")
    if session is None:
        session_id = UNATTRIBUTED_SESSION
    else:
        session_id = _required_string(session, where + ".sessionId")
        if _SAFE_ID.fullmatch(session_id) is None:
            raise ClaudeHistoryParseError(f"{where}.sessionId: unsafe session id {session_id!r}")
    pasted = _pasted_text(value.get("pastedContents"), where)
    text = display if not pasted else f"{display}\n\n{pasted}"
    return _HistoryPrompt(line, timestamp, session_id, project, text)


def read_history_prompts(path: Path) -> tuple[tuple[_HistoryPrompt, ...], bytes]:
    """Parse every newline-complete history line; return the prompts and the bytes they came from."""

    data = path.read_bytes()
    complete = _complete_jsonl(data)
    if not complete:
        raise ClaudeHistoryParseError(f"{path}: no newline-complete JSON records")
    prompts: list[_HistoryPrompt] = []
    for line, raw in enumerate(complete.split(b"\n")[:-1], start=1):
        if not raw.strip():
            continue
        prompts.append(_parse_prompt(raw, line, f"{path}:{line}"))
    return tuple(prompts), complete


def snapshot_claude_history(
    history_file: Path,
    snapshot_root: Path,
    previous: ClaudeSourceCopy | None,
    updated_at: str,
) -> ClaudeHistorySnapshotResult:
    """Copy the newline-complete prefix of the history file, refusing a rewrite of what was copied.

    The history is an append-only log in practice, and this holds it to that: a copy that is no
    longer a prefix of the live file is the file having been truncated or edited, and the archive
    then keeps the copy it has rather than silently replacing it.
    """

    relative = history_file.name
    if "/" in relative or relative in ("", ".", ".."):
        raise ClaudeHistoryParseError(f"unsafe history file name {relative!r}")
    data = history_file.read_bytes()
    complete = _complete_jsonl(data)
    if not complete:
        raise ClaudeHistoryParseError(f"{history_file}: no newline-complete JSON records")
    target = snapshot_root / relative
    if target.is_symlink():
        raise ClaudeHistoryParseError(f"history snapshot target is a symlink: {target}")
    existing = target.read_bytes() if target.is_file() else b""
    if previous is not None:
        if previous.source_path != relative:
            raise ClaudeHistoryParseError(
                "append-only source violation: previously observed history file "
                f"{previous.source_path!r} is not {relative!r}"
            )
        if len(existing) < previous.copied_bytes:
            raise ClaudeHistoryParseError(f"history snapshot is shorter than its manifest: {target}")
        if hashlib.sha256(existing[: previous.copied_bytes]).hexdigest() != previous.sha256:
            raise ClaudeHistoryParseError(f"history snapshot differs from its manifest: {target}")
    if len(complete) < len(existing) or complete[: len(existing)] != existing:
        raise ClaudeHistoryParseError(
            f"append-only source violation: history file was truncated or rewritten: {relative}"
        )
    changed = int(write_text_if_changed(target, complete.decode("utf-8")))
    digest = hashlib.sha256(complete).hexdigest()
    copy_updated = (
        previous.updated_at
        if previous is not None
        and previous.copied_bytes == len(complete)
        and previous.sha256 == digest
        else updated_at
    )
    return ClaudeHistorySnapshotResult(
        ClaudeSourceCopy(
            source_path=relative,
            original_path=str(history_file.resolve()),
            snapshot_path=relative,
            thread_id=_root_thread_id(relative),
            copied_bytes=len(complete),
            line_count=complete.count(b"\n"),
            sha256=digest,
            updated_at=copy_updated,
        ),
        changed,
    )


def _root_thread_id(relative: str) -> str:
    """The team's root thread is the history file itself, named safely."""

    stem = PurePosixPath(relative).stem or "history"
    candidate = re.sub(r"[^A-Za-z0-9._-]", "-", stem)
    if _SAFE_ID.fullmatch(candidate) is None:
        candidate = "history"
    return candidate


def load_claude_history_team(
    snapshot_file: Path,
    team_slug: str,
    display_timezone: str,
    *,
    project_pattern: str,
    covered_session_ids: Sequence[str] = (),
) -> tuple[TeamData, ClaudeHistorySelection]:
    """Normalize the prompts whose working directory matches, one coordinator agent per session.

    ``project_pattern`` is searched (not anchored) against each prompt's ``project`` field;
    ``covered_session_ids`` names sessions whose full transcript is archived elsewhere and whose
    prompts are therefore left out here. Both decisions are the operator's, and both are reported
    back so a receipt can say what was excluded and why.
    """

    pattern = compile_project_pattern(project_pattern)
    covered = frozenset(covered_session_ids)
    prompts, complete = read_history_prompts(snapshot_file)
    relative = snapshot_file.name
    matched: list[_HistoryPrompt] = []
    covered_prompts = 0
    covered_sessions: set[str] = set()
    unmatched = 0
    for prompt in prompts:
        if pattern.search(prompt.project) is None:
            unmatched += 1
            continue
        if prompt.session_id in covered:
            covered_prompts += 1
            covered_sessions.add(prompt.session_id)
            continue
        matched.append(prompt)
    if not matched:
        raise ClaudeHistoryParseError(
            f"no history prompt matched project pattern {project_pattern!r}"
            + (f" outside the {len(covered_sessions)} covered session(s)" if covered_sessions else "")
        )

    events: list[Event] = []
    turns: list[Turn] = []
    by_session: dict[str, list[_HistoryPrompt]] = {}
    for prompt in matched:
        by_session.setdefault(prompt.session_id, []).append(prompt)
        event_id = f"{prompt.session_id}:history:{prompt.line}"
        events.append(
            Event(
                event_id=event_id,
                thread_id=prompt.session_id,
                turn_id=event_id,
                timestamp_ms=prompt.timestamp_ms,
                kind="user_prompt",
                role="user",
                phase=None,
                text=prompt.text,
                content_availability="plain",
                encrypted_content=None,
                author="user",
                recipient=prompt.session_id,
                source_line=prompt.line,
                ingress_kind=INGRESS_KIND,
                author_kind="owner_human",
                source_native_id=f"{relative}:{prompt.line}",
                classification_version=_CLASSIFICATION_VERSION,
            )
        )
        turns.append(
            Turn(
                turn_id=event_id,
                thread_id=prompt.session_id,
                started_at_ms=prompt.timestamp_ms,
                ended_at_ms=prompt.timestamp_ms + _PLACEHOLDER_TURN_MS,
                status="unknown",
                first_token_ms=None,
                error=None,
                last_agent_message=None,
            )
        )

    agents: list[Agent] = []
    for session_id in sorted(by_session, key=lambda item: (by_session[item][0].timestamp_ms, item)):
        times = [prompt.timestamp_ms for prompt in by_session[session_id]]
        agents.append(
            Agent(
                thread_id=session_id,
                parent_thread_id=None,
                agent_path=f"/{session_id}",
                nickname=None,
                role="coordinator",
                depth=0,
                started_at_ms=min(times),
                ended_at_ms=max(times) + _PLACEHOLDER_TURN_MS,
                status="unknown",
                source_path=relative,
            )
        )
    metadata = snapshot_file.stat()
    source = SourceSnapshot(
        path=relative,
        thread_id=agents[0].thread_id,
        size_bytes=metadata.st_size,
        mtime_ns=metadata.st_mtime_ns,
        sha256=hashlib.sha256(complete).hexdigest(),
        complete_bytes=len(complete),
        line_count=complete.count(b"\n"),
    )
    team = TeamData(
        team_slug=team_slug,
        provider=PROVIDER,
        root_thread_id=agents[0].thread_id,
        display_timezone=display_timezone,
        sources=(source,),
        agents=tuple(agents),
        turns=tuple(sorted(turns, key=lambda turn: (turn.started_at_ms, turn.turn_id))),
        events=tuple(sorted(events, key=lambda event: (event.timestamp_ms, event.event_id))),
        tool_calls=(),
        edges=(),
    )
    return team, ClaudeHistorySelection(
        matched_prompts=len(matched),
        covered_prompts=covered_prompts,
        covered_sessions=len(covered_sessions),
        unmatched_prompts=unmatched,
    )


__all__ = [
    "ClaudeHistoryParseError",
    "ClaudeHistorySelection",
    "ClaudeHistorySnapshotResult",
    "INGRESS_KIND",
    "PROVIDER",
    "UNATTRIBUTED_SESSION",
    "compile_project_pattern",
    "load_claude_history_team",
    "read_history_prompts",
    "snapshot_claude_history",
]

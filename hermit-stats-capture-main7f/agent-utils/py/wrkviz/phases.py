"""Stable time windows, activity states, transcript views, and summary inputs."""

from __future__ import annotations

import hashlib
from bisect import bisect_left
from collections import Counter, defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime, timezone

from wrkviz.model import Agent, Event, TeamData, ToolCall, Turn
from wrkviz.summarize import SummaryJob


@dataclass(frozen=True)
class PhaseStats:
    """Activity counts collected for one or more timeline phases."""

    user_prompts: int
    agent_responses: int
    inter_agent_messages: int
    tool_calls: int
    external_messages: int = 0

    def to_mapping(self) -> dict[str, int]:
        """Return the counters keyed by their stable archive field names."""

        result = {
            "user_prompts": self.user_prompts,
            "agent_responses": self.agent_responses,
            "inter_agent_messages": self.inter_agent_messages,
            "tool_calls": self.tool_calls,
        }
        if self.external_messages:
            result["external_messages"] = self.external_messages
        return result


@dataclass(frozen=True)
class StateSegment:
    """A contiguous interval with one derived agent activity state."""

    start_ms: int
    end_ms: int
    kind: str

    def to_json_obj(self) -> dict[str, object]:
        """Return the state segment as a JSON-serializable object."""

        return {"start_ms": self.start_ms, "end_ms": self.end_ms, "kind": self.kind}


@dataclass(frozen=True)
class TranscriptTool:
    """A tool name and its collapsed invocation count in a transcript entry."""

    name: str
    count: int

    def to_json_obj(self) -> dict[str, object]:
        """Return the collapsed tool count as a JSON-serializable object."""

        return {"name": self.name, "count": self.count}


@dataclass(frozen=True)
class TranscriptEntry:
    """One timeline transcript item with any adjacent tool calls."""

    at_ms: int
    role: str
    text: str
    tools: tuple[TranscriptTool, ...]

    def to_json_obj(self) -> dict[str, object]:
        """Return the transcript entry as a JSON-serializable object."""

        return {
            "at_ms": self.at_ms,
            "role": self.role,
            "text": self.text,
            "tools": [tool.to_json_obj() for tool in self.tools],
        }


@dataclass(frozen=True)
class PhaseWindow:
    """A bounded agent-work interval and the material needed to summarize it."""

    phase_id: str
    summary_key: str
    agent_id: str
    agent_label: str
    start_ms: int
    end_ms: int
    stats: PhaseStats
    states: tuple[StateSegment, ...]
    transcript_text: str
    prior_context: str
    transcript: tuple[TranscriptEntry, ...]


_TEXT_KINDS = frozenset(
    {
        "user_prompt",
        "assistant_message",
        "inter_agent_message",
        "external_message",
        "goal_updated",
    }
)
_WAIT_TOOLS = frozenset({"wait", "wait_agent"})


def agent_label(agent: Agent) -> str:
    """Stable human label that keeps the descriptive path visible."""

    if agent.agent_path == "/root":
        return "Coordinator"
    slug = agent.agent_path.rsplit("/", 1)[-1].replace("_", "-")
    readable = " ".join(part for part in slug.split("-") if part)
    if agent.nickname:
        return f"{readable} · {agent.nickname}"
    return readable


def _iso(at_ms: int) -> str:
    return datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )


def _text(event: Event) -> str:
    if event.content_availability == "encrypted":
        message_type = "message"
        for line in (event.text or "").splitlines():
            if line.startswith("Message Type:"):
                candidate = line.partition(":")[2].strip().lower().replace("_", " ")
                if candidate:
                    message_type = candidate
                break
        route = ""
        if event.author or event.recipient:
            route = f" from {event.author or 'unknown'} to {event.recipient or 'unknown'}"
        return (
            f"[Encrypted Codex collaboration {message_type}{route}; "
            "message body unavailable offline.]"
        )
    if event.text:
        return event.text.strip()
    return ""


def _tool_count(tool: ToolCall) -> int:
    nested = sum(count for _, count in tool.nested_tools)
    return nested if nested else 1


def _tool_names(tool: ToolCall) -> tuple[TranscriptTool, ...]:
    if tool.nested_tools:
        return tuple(TranscriptTool(name, count) for name, count in tool.nested_tools)
    return (TranscriptTool(tool.name, 1),)


def _thread_end(
    agent: Agent,
    events: Sequence[Event],
    tools: Sequence[ToolCall],
    turns: Sequence[Turn],
) -> int:
    candidates = [agent.started_at_ms + 1000]
    if agent.ended_at_ms is not None:
        candidates.append(agent.ended_at_ms)
    # Phase intervals are half-open.  Make an instantaneous transcript event extend the
    # derived lifetime through the millisecond containing that event instead of choosing the
    # event timestamp itself as an exclusive bound and silently dropping the final message.
    candidates.extend(event.timestamp_ms + 1 for event in events)
    candidates.extend(
        (tool.ended_at_ms or tool.started_at_ms) + 1
        for tool in tools
    )
    candidates.extend(
        (turn.ended_at_ms or turn.started_at_ms)
        for turn in turns
    )
    return max(candidates)


def _blocked_ranges(events: Sequence[Event], end_ms: int) -> tuple[tuple[int, int], ...]:
    result: list[tuple[int, int]] = []
    ordered = sorted(events, key=lambda event: (event.timestamp_ms, event.event_id))
    for index, event in enumerate(ordered):
        if event.kind != "goal_updated" or not (event.text or "").lower().startswith("blocked"):
            continue
        stop = end_ms
        for later in ordered[index + 1 :]:
            if later.kind in ("user_prompt", "assistant_message", "goal_updated"):
                stop = later.timestamp_ms
                break
        if stop > event.timestamp_ms:
            result.append((event.timestamp_ms, stop))
    return tuple(result)


def _contains(start: int, end: int, midpoint: float) -> bool:
    return start <= midpoint < end


def _states(
    start_ms: int,
    end_ms: int,
    turns: Sequence[Turn],
    tools: Sequence[ToolCall],
    blocked: Sequence[tuple[int, int]],
) -> tuple[StateSegment, ...]:
    points = {start_ms, end_ms}
    for turn in turns:
        points.add(max(start_ms, turn.started_at_ms))
        points.add(min(end_ms, turn.ended_at_ms or end_ms))
    for tool in tools:
        points.add(max(start_ms, tool.started_at_ms))
        points.add(min(end_ms, tool.ended_at_ms or end_ms))
    for left, right in blocked:
        points.add(max(start_ms, left))
        points.add(min(end_ms, right))
    boundaries = sorted(point for point in points if start_ms <= point <= end_ms)
    raw: list[StateSegment] = []
    for left, right in zip(boundaries, boundaries[1:]):
        if right <= left:
            continue
        midpoint = (left + right) / 2
        kind = "idle"
        if any(_contains(block_left, block_right, midpoint) for block_left, block_right in blocked):
            kind = "blocked"
        matching_tools = [
            tool
            for tool in tools
            if _contains(tool.started_at_ms, tool.ended_at_ms or end_ms, midpoint)
        ]
        if matching_tools:
            kind = (
                "waiting"
                if any(tool.name in _WAIT_TOOLS for tool in matching_tools)
                else "tool"
            )
        elif any(
            _contains(turn.started_at_ms, turn.ended_at_ms or end_ms, midpoint)
            for turn in turns
        ):
            kind = "active"
        if raw and raw[-1].kind == kind and raw[-1].end_ms == left:
            previous = raw[-1]
            raw[-1] = StateSegment(previous.start_ms, right, kind)
        else:
            raw.append(StateSegment(left, right, kind))
    return tuple(raw)


def _transcript_entries(
    events: Sequence[Event], tools: Sequence[ToolCall]
) -> tuple[TranscriptEntry, ...]:
    records: list[tuple[int, int, Event | ToolCall]] = []
    records.extend((event.timestamp_ms, 1, event) for event in events if event.kind in _TEXT_KINDS)
    records.extend((tool.started_at_ms, 0, tool) for tool in tools)
    records.sort(key=lambda item: (item[0], item[1]))
    result: list[TranscriptEntry] = []
    pending_at: int | None = None
    pending: Counter[str] = Counter()

    def flush_tools() -> None:
        nonlocal pending_at
        if pending_at is None:
            return
        tools_tuple = tuple(
            TranscriptTool(name, count) for name, count in sorted(pending.items())
        )
        count = sum(pending.values())
        noun = "tool" if count == 1 else "tools"
        result.append(TranscriptEntry(pending_at, "tool", f"{count} {noun} used", tools_tuple))
        pending.clear()
        pending_at = None

    for at_ms, _, record in records:
        if isinstance(record, ToolCall):
            if pending_at is None:
                pending_at = at_ms
            for tool in _tool_names(record):
                pending[tool.name] += tool.count
            continue
        flush_tools()
        text = _text(record)
        if not text:
            continue
        role_by_kind = {
            "user_prompt": "user",
            "assistant_message": "assistant",
            "inter_agent_message": "agent",
            "external_message": "external",
            "goal_updated": "goal",
        }
        result.append(
            TranscriptEntry(at_ms, role_by_kind.get(record.kind, "event"), text, ())
        )
    flush_tools()
    return tuple(result)


def _summary_text(entries: Sequence[TranscriptEntry], max_chars: int) -> str:
    lines: list[str] = []
    for entry in entries:
        if entry.role == "tool":
            detail = ", ".join(f"{tool.count} {tool.name}" for tool in entry.tools)
            lines.append(f"[{_iso(entry.at_ms)}] TOOLS: {detail}")
        else:
            lines.append(f"[{_iso(entry.at_ms)}] {entry.role.upper()}: {entry.text}")
    text = "\n\n".join(lines)
    if len(text) <= max_chars:
        return text
    front = max_chars // 3
    back = max_chars - front
    return text[:front] + "\n\n[...middle omitted for summary input...]\n\n" + text[-back:]


def _ancestor_ids(
    root_thread_id: str, by_id: Mapping[str, Agent], agent: Agent
) -> set[str]:
    ids = {agent.thread_id}
    parent = agent.parent_thread_id
    while parent is not None and parent in by_id:
        ids.add(parent)
        parent = by_id[parent].parent_thread_id
    ids.add(root_thread_id)
    return ids


def _prior_context(
    ancestor_ids: set[str],
    context_by_thread: Mapping[str, Sequence[tuple[int, str, str]]],
    start_ms: int,
    max_chars: int,
) -> str:
    candidates: list[tuple[Sequence[tuple[int, str, str]], int]] = []
    for thread_id in ancestor_ids:
        entries = context_by_thread.get(thread_id, ())
        cutoff = bisect_left(entries, (start_ms, "", ""))
        if cutoff:
            candidates.append((entries, cutoff - 1))

    reverse_lines: list[str] = []
    retained_chars = 0
    while candidates and retained_chars < max_chars:
        selected_index = max(
            range(len(candidates)),
            key=lambda index: candidates[index][0][candidates[index][1]][:2],
        )
        entries, entry_index = candidates[selected_index]
        reverse_lines.append(entries[entry_index][2])
        retained_chars += len(reverse_lines[-1])
        if len(reverse_lines) > 1:
            retained_chars += 2
        if entry_index == 0:
            candidates.pop(selected_index)
        else:
            candidates[selected_index] = (entries, entry_index - 1)

    text = "\n\n".join(reversed(reverse_lines))
    return text[-max_chars:]


def _in_window(team: TeamData, timestamp_ms: int) -> bool:
    if team.window_start_ms is not None and timestamp_ms < team.window_start_ms:
        return False
    return team.window_end_ms is None or timestamp_ms < team.window_end_ms


def _agent_overlaps_window(team: TeamData, agent: Agent) -> bool:
    """Return whether an agent's lifetime intersects the selected half-open range."""

    if team.window_end_ms is not None and agent.started_at_ms >= team.window_end_ms:
        return False
    if team.window_start_ms is None or agent.ended_at_ms is None:
        return True
    effective_end = max(agent.ended_at_ms, agent.started_at_ms + 1)
    return effective_end > team.window_start_ms


def phase_agent_ids(
    team: TeamData, phases: Sequence[PhaseWindow]
) -> frozenset[str]:
    """Return agents alive in the window plus their available lineage ancestors."""

    by_id = {agent.thread_id: agent for agent in team.agents}
    selected = {
        agent.thread_id for agent in team.agents if _agent_overlaps_window(team, agent)
    }
    selected.update(phase.agent_id for phase in phases)
    pending = list(selected)
    while pending:
        current = by_id.get(pending.pop())
        if current is None or current.parent_thread_id is None:
            continue
        if current.parent_thread_id in by_id and current.parent_thread_id not in selected:
            selected.add(current.parent_thread_id)
            pending.append(current.parent_thread_id)
    return frozenset(selected)


def build_phases(
    team: TeamData,
    *,
    phase_minutes: int = 30,
    context_chars: int = 16_000,
    transcript_chars: int = 30_000,
) -> tuple[PhaseWindow, ...]:
    """Partition activity into fixed, append-stable UTC windows."""

    if phase_minutes <= 0:
        raise ValueError("phase_minutes must be positive")
    if context_chars <= 0:
        raise ValueError("context_chars must be positive")
    if transcript_chars <= 0:
        raise ValueError("transcript_chars must be positive")
    window_ms = phase_minutes * 60 * 1000
    events_by_thread: dict[str, list[Event]] = defaultdict(list)
    tools_by_thread: dict[str, list[ToolCall]] = defaultdict(list)
    turns_by_thread: dict[str, list[Turn]] = defaultdict(list)
    for event in team.events:
        events_by_thread[event.thread_id].append(event)
    for tool in team.tool_calls:
        tools_by_thread[tool.thread_id].append(tool)
    for turn in team.turns:
        turns_by_thread[turn.thread_id].append(turn)
    for own_events in events_by_thread.values():
        own_events.sort(key=lambda event: (event.timestamp_ms, event.event_id))
    for own_tools in tools_by_thread.values():
        own_tools.sort(key=lambda tool: (tool.started_at_ms, tool.call_id))
    for own_turns in turns_by_thread.values():
        own_turns.sort(key=lambda turn: (turn.started_at_ms, turn.turn_id))

    context_by_thread: dict[str, list[tuple[int, str, str]]] = defaultdict(list)
    for event in team.events:
        rendered = _text(event)
        if event.kind not in _TEXT_KINDS or not rendered:
            continue
        context_by_thread[event.thread_id].append(
            (
                event.timestamp_ms,
                event.event_id,
                f"[{_iso(event.timestamp_ms)}] {event.kind}: {rendered}",
            )
        )
    for context_entries in context_by_thread.values():
        context_entries.sort(key=lambda item: (item[0], item[1]))
    agents_by_id = {agent.thread_id: agent for agent in team.agents}

    result: list[PhaseWindow] = []
    for agent in team.agents:
        own_events = events_by_thread.get(agent.thread_id, [])
        own_tools = tools_by_thread.get(agent.thread_id, [])
        own_turns = turns_by_thread.get(agent.thread_id, [])
        end_ms = _thread_end(agent, own_events, own_tools, own_turns)
        if team.window_end_ms is not None:
            end_ms = min(end_ms, team.window_end_ms)
        ancestor_ids = _ancestor_ids(team.root_thread_id, agents_by_id, agent)
        activity_times = [
            event.timestamp_ms
            for event in own_events
            if event.kind in _TEXT_KINDS and _in_window(team, event.timestamp_ms)
        ] + [tool.started_at_ms for tool in own_tools if _in_window(team, tool.started_at_ms)]
        if not activity_times:
            continue
        bucket_starts = sorted({(at_ms // window_ms) * window_ms for at_ms in activity_times})
        blocked = _blocked_ranges(own_events, end_ms)
        for bucket in bucket_starts:
            start_ms = max(
                agent.started_at_ms,
                bucket,
                team.window_start_ms if team.window_start_ms is not None else agent.started_at_ms,
            )
            phase_end = min(end_ms, bucket + window_ms)
            if phase_end <= start_ms:
                phase_end = start_ms + 1000
            phase_events = [
                event
                for event in own_events
                if start_ms <= event.timestamp_ms < phase_end
            ]
            phase_tools = [
                tool
                for tool in own_tools
                if start_ms <= tool.started_at_ms < phase_end
                or (
                    tool.started_at_ms < start_ms
                    and (tool.ended_at_ms or phase_end) > start_ms
                )
            ]
            phase_turns = [
                turn
                for turn in own_turns
                if turn.started_at_ms < phase_end
                and (turn.ended_at_ms or phase_end) > start_ms
            ]
            # Spanning tools still color this phase's state strip, but their invocation belongs
            # to the phase where it started. Keeping them out of later transcript slices prevents
            # a clicked window from displaying entries timestamped before its own boundary.
            started_phase_tools = [
                tool for tool in phase_tools if start_ms <= tool.started_at_ms < phase_end
            ]
            entries = _transcript_entries(phase_events, started_phase_tools)
            counts = Counter(event.kind for event in phase_events)
            stats = PhaseStats(
                user_prompts=counts["user_prompt"],
                agent_responses=counts["assistant_message"],
                inter_agent_messages=counts["inter_agent_message"],
                # A tool that spans a phase boundary participates in both phases' state
                # rendering, but belongs to exactly one phase for aggregate accounting.
                tool_calls=sum(_tool_count(tool) for tool in started_phase_tools),
                external_messages=counts["external_message"],
            )
            stable = f"{team.team_slug}\0{agent.thread_id}\0{bucket}\0{phase_minutes}"
            suffix = hashlib.sha256(stable.encode("utf-8")).hexdigest()[:16]
            phase_id = f"phase-{suffix}"
            result.append(
                PhaseWindow(
                    phase_id=phase_id,
                    summary_key=phase_id,
                    agent_id=agent.thread_id,
                    agent_label=agent_label(agent),
                    start_ms=start_ms,
                    end_ms=phase_end,
                    stats=stats,
                    states=_states(start_ms, phase_end, phase_turns, phase_tools, blocked),
                    transcript_text=_summary_text(entries, transcript_chars),
                    prior_context=_prior_context(
                        ancestor_ids,
                        context_by_thread,
                        start_ms,
                        context_chars,
                    ),
                    transcript=entries,
                )
            )
    return tuple(sorted(result, key=lambda item: (item.start_ms, item.agent_id, item.phase_id)))


def summary_jobs_for_phases(
    team: TeamData, phases: Sequence[PhaseWindow], glossary: str
) -> tuple[SummaryJob, ...]:
    """Create deterministic summary jobs for the supplied phase windows."""

    return tuple(
        SummaryJob(
            key=phase.summary_key,
            team_slug=team.team_slug,
            agent_label=phase.agent_label,
            start_ms=phase.start_ms,
            end_ms=phase.end_ms,
            prior_context=phase.prior_context,
            transcript=phase.transcript_text,
            glossary=glossary,
            stats=phase.stats.to_mapping(),
        )
        for phase in phases
    )


def aggregate_stats(phases: Sequence[PhaseWindow]) -> PhaseStats:
    """Sum activity counters across *phases*."""

    return PhaseStats(
        user_prompts=sum(phase.stats.user_prompts for phase in phases),
        agent_responses=sum(phase.stats.agent_responses for phase in phases),
        inter_agent_messages=sum(phase.stats.inter_agent_messages for phase in phases),
        tool_calls=sum(phase.stats.tool_calls for phase in phases),
        external_messages=sum(phase.stats.external_messages for phase in phases),
    )

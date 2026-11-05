"""Deterministic, zero-model transcript search records for static exports."""

from __future__ import annotations

from dataclasses import dataclass

from wrkviz.archive import JsonValue
from wrkviz.model import Event, TeamData, ToolCall


SEARCH_RECORD_SCHEMA_VERSION = 1

_TEXT_ROLES = {
    "user_prompt": "user",
    "assistant_message": "assistant",
    "inter_agent_message": "agent",
    "external_message": "external",
    "goal_updated": "goal",
}


@dataclass(frozen=True)
class _PromptLink:
    at_ms: int
    source_line: int
    event_id: str
    author_kind: str | None


@dataclass(frozen=True)
class _AgentRoutes:
    aliases_by_thread: dict[str, frozenset[str]]
    threads_by_alias: dict[str, frozenset[str]]
    parent_by_thread: dict[str, str | None]
    lifetime_by_thread: dict[str, tuple[int, int | None]]


def _message_ref(team_slug: str, event_id: str) -> str:
    return f"message:{team_slug}::{event_id}"


def _tool_ref(team_slug: str, call_id: str) -> str:
    return f"tool:{team_slug}::{call_id}"


def _agent_ref(team_slug: str, thread_id: str) -> str:
    return f"agent:{team_slug}::{thread_id}"


def _agent_id(team_slug: str, thread_id: str, namespace_agents: bool) -> str:
    return f"{team_slug}::{thread_id}" if namespace_agents else thread_id


def _event_text(event: Event) -> str:
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
    return (event.text or "").strip()


def _tool_text(tool: ToolCall) -> str:
    if tool.nested_tools:
        parts = [f"{count} {name}" for name, count in sorted(tool.nested_tools)]
        count = sum(value for _, value in tool.nested_tools)
    else:
        qualified = f"{tool.namespace}.{tool.name}" if tool.namespace else tool.name
        parts = [f"1 {qualified}"]
        count = 1
    noun = "tool" if count == 1 else "tools"
    return f"{count} {noun} used: " + ", ".join(parts)


def _agent_routes(team: TeamData) -> _AgentRoutes:
    continuation_prefixes = tuple(
        agent.agent_path.rstrip("/")
        for agent in team.agents
        if team.provider == "codex"
        and agent.parent_thread_id is not None
        and agent.role == "coordinator"
        and agent.agent_path.startswith("/root/continuation-")
    )
    aliases_by_thread: dict[str, frozenset[str]] = {}
    for agent in team.agents:
        aliases = {agent.thread_id, agent.agent_path}
        for prefix in continuation_prefixes:
            if agent.agent_path == prefix:
                aliases.add("/root")
            elif agent.agent_path.startswith(prefix + "/"):
                aliases.add("/root" + agent.agent_path[len(prefix) :])
        aliases_by_thread[agent.thread_id] = frozenset(aliases)
    threads_by_alias: dict[str, set[str]] = {}
    for thread_id, route_aliases in aliases_by_thread.items():
        for alias in route_aliases:
            threads_by_alias.setdefault(alias, set()).add(thread_id)
    return _AgentRoutes(
        aliases_by_thread=aliases_by_thread,
        threads_by_alias={
            alias: frozenset(thread_ids)
            for alias, thread_ids in threads_by_alias.items()
        },
        parent_by_thread={
            agent.thread_id: agent.parent_thread_id for agent in team.agents
        },
        lifetime_by_thread={
            agent.thread_id: (agent.started_at_ms, agent.ended_at_ms)
            for agent in team.agents
        },
    )


def _resolved_route(
    routes: _AgentRoutes, event: Event
) -> tuple[str | None, str | None]:
    authors = routes.threads_by_alias.get(event.author or "", frozenset())
    recipients = routes.threads_by_alias.get(event.recipient or "", frozenset())
    direct = [
        (author, recipient)
        for author in authors
        for recipient in recipients
        if routes.parent_by_thread.get(author) == recipient
        or routes.parent_by_thread.get(recipient) == author
    ]
    on_event_thread = [
        pair for pair in direct if event.thread_id in pair
    ]
    candidates = on_event_thread or direct
    if candidates:
        def route_score(pair: tuple[str, str]) -> tuple[int, int, str, str]:
            starts: list[int] = []
            active = 0
            for thread_id in pair:
                started_at_ms, ended_at_ms = routes.lifetime_by_thread[thread_id]
                starts.append(started_at_ms)
                if started_at_ms <= event.timestamp_ms and (
                    ended_at_ms is None or event.timestamp_ms <= ended_at_ms
                ):
                    active += 1
            return active, max(starts), pair[0], pair[1]

        return max(candidates, key=route_score)
    return (
        next(iter(authors)) if len(authors) == 1 else None,
        next(iter(recipients)) if len(recipients) == 1 else None,
    )


def _message_type(event: Event) -> str | None:
    for line in (event.text or "").splitlines():
        if not line.strip():
            continue
        label, separator, value = line.partition(":")
        if separator and label.strip().casefold() == "message type":
            return value.strip().casefold().replace("-", "_").replace(" ", "_")
        return None
    return None


def _inter_agent_record_type(
    routes: _AgentRoutes, event: Event
) -> str:
    author_thread, recipient_thread = _resolved_route(routes, event)
    if (
        author_thread is not None
        and recipient_thread is not None
        and routes.parent_by_thread.get(recipient_thread) == author_thread
    ):
        return "inter_agent_prompt"
    if (
        author_thread is not None
        and recipient_thread is not None
        and routes.parent_by_thread.get(author_thread) == recipient_thread
    ):
        return "inter_agent_response"
    message_type = _message_type(event)
    if event.phase == "instruction" or message_type in {
        "new_task",
        "followup_task",
    }:
        return "inter_agent_prompt"
    if event.phase == "final_answer" or message_type == "final_answer":
        return "inter_agent_response"
    aliases = routes.aliases_by_thread.get(
        event.thread_id, frozenset((event.thread_id,))
    )
    author_is_thread = event.author in aliases
    recipient_is_thread = event.recipient in aliases
    if recipient_is_thread and not author_is_thread:
        return "inter_agent_prompt"
    if author_is_thread and not recipient_is_thread:
        return "inter_agent_response"
    return "inter_agent"


def _record_type(
    routes: _AgentRoutes, event: Event
) -> str:
    if event.kind == "inter_agent_message":
        return _inter_agent_record_type(routes, event)
    return {
        "user_prompt": "prompt",
        "assistant_message": "response",
        "external_message": "external",
        "goal_updated": "goal",
        "system_input": "system",
    }.get(event.kind, event.kind)


def _record_author_kind(event: Event, record_type: str) -> str | None:
    if record_type in {
        "inter_agent",
        "inter_agent_prompt",
        "inter_agent_response",
    }:
        return event.author_kind or "agent"
    return event.author_kind


def _event_role(event: Event) -> str:
    explicit = _TEXT_ROLES.get(event.kind)
    if explicit is not None:
        return explicit
    if event.kind == "system_input":
        return "system"
    if event.kind.startswith("subagent_"):
        return "agent"
    if event.role in {"user", "assistant", "agent", "system", "external", "goal"}:
        return event.role
    return "event"


def _prompt_lists(
    team: TeamData,
    routes: _AgentRoutes,
) -> tuple[
    dict[tuple[str, str], tuple[_PromptLink, ...]],
    dict[str, tuple[_PromptLink, ...]],
    dict[tuple[str, str], tuple[_PromptLink, ...]],
]:
    by_turn_mutable: dict[tuple[str, str], list[_PromptLink]] = {}
    by_thread_mutable: dict[str, list[_PromptLink]] = {}
    by_route_mutable: dict[tuple[str, str], list[_PromptLink]] = {}
    for event in sorted(
        team.events, key=lambda item: (item.timestamp_ms, item.source_line, item.event_id)
    ):
        record_type = _record_type(routes, event)
        if record_type not in {"prompt", "inter_agent_prompt"}:
            continue
        link = _PromptLink(
            event.timestamp_ms,
            event.source_line,
            event.event_id,
            _record_author_kind(event, record_type),
        )
        by_thread_mutable.setdefault(event.thread_id, []).append(link)
        if event.turn_id is not None:
            by_turn_mutable.setdefault((event.thread_id, event.turn_id), []).append(link)
        route = _prompt_route(routes, event, record_type)
        if route is not None:
            by_route_mutable.setdefault(route, []).append(link)
    return (
        {
            key: tuple(
                sorted(
                    value,
                    key=lambda item: (item.at_ms, item.source_line, item.event_id),
                )
            )
            for key, value in by_turn_mutable.items()
        },
        {
            key: tuple(
                sorted(
                    value,
                    key=lambda item: (item.at_ms, item.source_line, item.event_id),
                )
            )
            for key, value in by_thread_mutable.items()
        },
        {
            key: tuple(
                sorted(
                    value,
                    key=lambda item: (item.at_ms, item.event_id),
                )
            )
            for key, value in by_route_mutable.items()
        },
    )


def _prompt_route(
    routes: _AgentRoutes, event: Event, record_type: str
) -> tuple[str, str] | None:
    author_thread, recipient_thread = _resolved_route(routes, event)
    if author_thread is None or recipient_thread is None:
        return None
    if (
        record_type == "inter_agent_prompt"
        and routes.parent_by_thread.get(recipient_thread) == author_thread
    ):
        return author_thread, recipient_thread
    if (
        record_type == "inter_agent_response"
        and routes.parent_by_thread.get(author_thread) == recipient_thread
    ):
        return recipient_thread, author_thread
    return None


def _record_agent_thread(
    routes: _AgentRoutes,
    event: Event,
    record_type: str,
    visible_agent_ids: frozenset[str],
) -> str:
    """Attribute routed work to the child that received or produced it.

    Codex stores collaboration messages on the receiving rollout.  A child return is therefore
    delivered on the parent thread even though the child is the author whose work users want to
    find and highlight.  Keep ambiguous/unavailable routes on their physical event thread.
    """

    author_thread, recipient_thread = _resolved_route(routes, event)
    candidate: str | None = None
    if record_type == "inter_agent_prompt":
        candidate = recipient_thread
    elif record_type == "inter_agent_response":
        candidate = author_thread
    if candidate in visible_agent_ids:
        return candidate
    return event.thread_id


def _linked_prompt(
    thread_id: str,
    turn_id: str | None,
    at_ms: int,
    source_line: int,
    stable_id: str,
    by_turn: dict[tuple[str, str], tuple[_PromptLink, ...]],
    by_thread: dict[str, tuple[_PromptLink, ...]],
    route: tuple[str, str] | None = None,
    by_route: dict[tuple[str, str], tuple[_PromptLink, ...]] | None = None,
) -> _PromptLink | None:
    if route is not None and by_route is not None:
        candidates = by_route.get(route, ())
    elif turn_id is not None:
        candidates = by_turn.get((thread_id, turn_id), ())
    else:
        candidates = by_thread.get(thread_id, ())
    for prompt in reversed(candidates):
        if route is not None:
            if prompt.at_ms < at_ms:
                return prompt
            continue
        if (prompt.at_ms, prompt.source_line, prompt.event_id) <= (
            at_ms,
            source_line,
            stable_id,
        ):
            return prompt
    return None


def _in_scope(
    at_ms: int, start_ms: int | None, end_ms: int | None
) -> bool:
    return (start_ms is None or at_ms >= start_ms) and (
        end_ms is None or at_ms < end_ms
    )


def build_search_records(
    team: TeamData,
    visible_agent_ids: frozenset[str],
    *,
    namespace_agents: bool = False,
    start_ms: int | None = None,
    end_ms: int | None = None,
) -> tuple[dict[str, JsonValue], ...]:
    """Build unique searchable transcript records from canonical events and tool calls.

    Prompt linkage is mechanical: parent/child returns use the latest strictly earlier instruction
    on that immutable route. Equal-time cross-rollout ordering is not causal and remains unlinked.
    Other responses and tools use their provider turn, or their thread only when no turn identity
    was recorded. Tool payloads remain excluded; their compact names/counts match the
    full-transcript UI.
    """

    if start_ms is not None and end_ms is not None and start_ms >= end_ms:
        raise ValueError("search record start must be earlier than end")
    agents = {agent.thread_id: agent for agent in team.agents}
    routes = _agent_routes(team)
    by_turn, by_thread, by_route = _prompt_lists(team, routes)
    sortable: list[tuple[int, int, str, dict[str, JsonValue]]] = []
    seen: set[str] = set()

    for event in team.events:
        role = _event_role(event)
        if (
            event.thread_id not in visible_agent_ids
            or not _in_scope(event.timestamp_ms, start_ms, end_ms)
        ):
            continue
        text = _event_text(event)
        if not text:
            continue
        reference = _message_ref(team.team_slug, event.event_id)
        if reference in seen:
            raise ValueError(f"duplicate transcript search record {reference}")
        seen.add(reference)
        event_record_type = _record_type(routes, event)
        event_author_kind = _record_author_kind(event, event_record_type)
        route = _prompt_route(routes, event, event_record_type)
        record_thread_id = _record_agent_thread(
            routes, event, event_record_type, visible_agent_ids
        )
        prompt = (
            _PromptLink(
                event.timestamp_ms,
                event.source_line,
                event.event_id,
                event_author_kind,
            )
            if event_record_type in {"prompt", "inter_agent_prompt"}
            else _linked_prompt(
                event.thread_id,
                event.turn_id,
                event.timestamp_ms,
                event.source_line,
                event.event_id,
                by_turn,
                by_thread,
                route,
                by_route,
            )
        )
        agent = agents.get(record_thread_id)
        record: dict[str, JsonValue] = {
            "schema_version": SEARCH_RECORD_SCHEMA_VERSION,
            "ref": reference,
            "record_type": event_record_type,
            "role": role,
            "team": team.team_slug,
            "agent_id": _agent_id(
                team.team_slug, record_thread_id, namespace_agents
            ),
            "agent_ref": _agent_ref(team.team_slug, record_thread_id),
            "agent_path": agent.agent_path if agent is not None else record_thread_id,
            "event_id": event.event_id,
            "turn_id": event.turn_id,
            "at_ms": event.timestamp_ms,
            "text": text,
            "author_kind": event_author_kind,
            "ingress_kind": event.ingress_kind,
            "author": event.author,
            "recipient": event.recipient,
            "phase": event.phase,
            "prompt_ref": (
                _message_ref(team.team_slug, prompt.event_id)
                if prompt is not None
                else None
            ),
            "prompt_author_kind": prompt.author_kind if prompt is not None else None,
            "prompt_at_ms": prompt.at_ms if prompt is not None else None,
            "prompt_in_scope": (
                prompt is not None and _in_scope(prompt.at_ms, start_ms, end_ms)
            ),
            "content_fidelity": (
                "encrypted-placeholder"
                if event.content_availability == "encrypted"
                else "verbatim"
            ),
        }
        sortable.append((event.timestamp_ms, 1, reference, record))

    for tool in team.tool_calls:
        if (
            tool.thread_id not in visible_agent_ids
            or not _in_scope(tool.started_at_ms, start_ms, end_ms)
        ):
            continue
        reference = _tool_ref(team.team_slug, tool.call_id)
        if reference in seen:
            raise ValueError(f"duplicate transcript search record {reference}")
        seen.add(reference)
        prompt = _linked_prompt(
            tool.thread_id,
            tool.turn_id,
            tool.started_at_ms,
            tool.source_line,
            tool.call_id,
            by_turn,
            by_thread,
        )
        agent = agents.get(tool.thread_id)
        record = {
            "schema_version": SEARCH_RECORD_SCHEMA_VERSION,
            "ref": reference,
            "record_type": "tool",
            "role": "tool",
            "team": team.team_slug,
            "agent_id": _agent_id(
                team.team_slug, tool.thread_id, namespace_agents
            ),
            "agent_ref": _agent_ref(team.team_slug, tool.thread_id),
            "agent_path": agent.agent_path if agent is not None else tool.thread_id,
            "event_id": tool.call_id,
            "turn_id": tool.turn_id,
            "at_ms": tool.started_at_ms,
            "text": _tool_text(tool),
            "author_kind": "agent",
            "ingress_kind": "tool",
            "prompt_ref": (
                _message_ref(team.team_slug, prompt.event_id)
                if prompt is not None
                else None
            ),
            "prompt_author_kind": prompt.author_kind if prompt is not None else None,
            "prompt_at_ms": prompt.at_ms if prompt is not None else None,
            "prompt_in_scope": (
                prompt is not None and _in_scope(prompt.at_ms, start_ms, end_ms)
            ),
            "content_fidelity": "condensed",
        }
        sortable.append((tool.started_at_ms, 0, reference, record))

    sortable.sort(key=lambda item: item[:3])
    return tuple(record for _, _, _, record in sortable)


__all__ = ["SEARCH_RECORD_SCHEMA_VERSION", "build_search_records"]

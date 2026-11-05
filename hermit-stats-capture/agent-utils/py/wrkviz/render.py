"""Pure formatting of cached normalized data into Markdown and a static website."""

from __future__ import annotations

import os
from collections.abc import Mapping, Sequence
from datetime import datetime, timezone
from importlib.resources import files
from pathlib import Path
from zoneinfo import ZoneInfo

from wrkviz.activity_bins import build_activity_bins
from wrkviz.archive import (
    JsonValue,
    as_array,
    as_object,
    as_string,
    canonical_json,
    narrow_json,
    read_json,
    write_text_if_changed,
)
from wrkviz.artifacts import (
    ArtifactCatalog,
    ArtifactRangeIndex,
)
from wrkviz.github_refs import find_pull_request_references
from wrkviz.identity import SiteIdentity
from wrkviz.github_metadata import PullRequestKey, PullRequestMetadata
from wrkviz.model import Agent, Edge, Event, TeamData, Turn, source_digest
from wrkviz.naming import AgentNameResult
from wrkviz.periods import Period, period_heading
from wrkviz.phases import (
    PhaseStats,
    PhaseWindow,
    TranscriptEntry,
    phase_agent_ids,
)
from wrkviz.search_index import build_search_records
from wrkviz.summarize import (
    SummaryResult,
    clean_summary_prose,
    clean_summary_result,
)
from wrkviz.static_assets import (
    gzip_sidecar_path,
    sync_gzip_sidecar,
    write_gzip_only,
    write_text_with_gzip_invalidation,
)
from wrkviz.timeline_shards import (
    SCHEMA_2_BOOTSTRAP_PATH,
    schema_2_is_published,
)
from wrkviz.timeline_v3 import is_schema_3_path, write_timeline_v3
from wrkviz.window import DateWindow
from wrkviz.terminology import (
    GlossaryTerm,
    glossary_catalog_markdown,
    glossary_markdown,
)


def _iso(at_ms: int) -> str:
    return datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )


def _local_time(at_ms: int, display_timezone: str) -> str:
    local = datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc).astimezone(
        ZoneInfo(display_timezone)
    )
    return local.strftime("%Y-%m-%d %H:%M:%S %Z")


def _summary_obj(result: SummaryResult) -> dict[str, object]:
    cleaned = clean_summary_result(result)
    return {
        "key": cleaned.key,
        "phrase": cleaned.phrase,
        "paragraph": cleaned.paragraph,
        "work_summary": [
            {"at_ms": bullet.at_ms, "text": bullet.text}
            for bullet in cleaned.work_summary
        ],
        "model": cleaned.model,
        "prompt_version": cleaned.prompt_version,
        "input_hash": cleaned.input_hash,
        "generated_at": cleaned.generated_at,
        "summary_available": cleaned.summary_available,
    }


def _agent_name(
    agent: Agent, names: Mapping[str, AgentNameResult]
) -> AgentNameResult:
    try:
        return names[agent.thread_id]
    except KeyError as error:
        raise ValueError(f"missing hindsight name for agent {agent.agent_path}") from error


def _official_leaf(agent: Agent) -> str:
    return agent.agent_path.rstrip("/").rsplit("/", 1)[-1] or "root"


def _agent_summary_available(name: AgentNameResult) -> bool:
    return name.summary_available and bool(
        clean_summary_prose(name.lifetime_summary or "")
    )


def _agent_identity_obj(agent: Agent, name: AgentNameResult) -> dict[str, object]:
    return {
        "short_name": name.short_name,
        "official_name": agent.agent_path,
        "official_leaf": _official_leaf(agent),
        "coordinator_nickname": agent.nickname or "",
        "naming_rationale": name.rationale,
        "lifetime_summary": clean_summary_prose(name.lifetime_summary or ""),
        "naming_model": name.model,
        "naming_input_hash": name.input_hash,
        "summary_available": _agent_summary_available(name),
    }


def _phase_markdown(
    team: TeamData,
    agent: Agent,
    name: AgentNameResult,
    phase: PhaseWindow,
    summary: SummaryResult,
) -> str:
    stats = phase.stats
    lines = [
        f"# {summary.phrase}",
        "",
        f"Team: `{team.team_slug}`  ",
        f"Agent: `{name.short_name}`  ",
        f"Official agent path: `{agent.agent_path}`  ",
        f"Window: {_local_time(phase.start_ms, team.display_timezone)} to "
        f"{_local_time(phase.end_ms, team.display_timezone)}",
        "",
        f"> {summary.paragraph}",
        "",
        "## Agent work summary",
        "",
    ]
    if summary.work_summary:
        lines.extend(
            f"- **{_local_time(item.at_ms, team.display_timezone)}** — {item.text}"
            for item in summary.work_summary
        )
    else:
        lines.append("_No substantive work item survived the archival filter._")
    lines.extend(
        [
            "",
            "## Window statistics",
            "",
            f"- User prompts: {stats.user_prompts}",
            f"- Agent responses: {stats.agent_responses}",
            f"- Inter-agent messages: {stats.inter_agent_messages}",
            *(
                [f"- External messages: {stats.external_messages}"]
                if stats.external_messages
                else []
            ),
            f"- Tool calls: {stats.tool_calls}",
            "",
            "## Summary provenance",
            "",
            f"- Model: `{summary.model}`",
            f"- Prompt: `{summary.prompt_version}`",
            f"- Input hash: `{summary.input_hash}`",
        ]
    )
    return "\n".join(lines).rstrip() + "\n"


def _rollup_markdown(
    team: TeamData,
    period: Period,
    summary: SummaryResult,
    stats: PhaseStats,
    audience: str,
) -> str:
    lines = [
        f"# {period_heading(period, team.display_timezone)} {team.team_slug} "
        f"{period.kind} {audience} summary",
        "",
        f"> **{summary.phrase}.** {summary.paragraph}",
        "",
        "## Agent work summary",
        "",
    ]
    if summary.work_summary:
        lines.extend(
            f"- **{_local_time(item.at_ms, team.display_timezone)}** — {item.text}"
            for item in summary.work_summary
        )
    else:
        lines.append("_No substantive work item was recorded in this range._")
    lines.extend(
        [
            "",
            "## Aggregate statistics",
            "",
            f"- User prompts: {stats.user_prompts}",
            f"- Agent responses: {stats.agent_responses}",
            f"- Inter-agent messages: {stats.inter_agent_messages}",
            *(
                [f"- External messages: {stats.external_messages}"]
                if stats.external_messages
                else []
            ),
            f"- Tool calls: {stats.tool_calls}",
            "",
            "## Summary provenance",
            "",
            f"Model `{summary.model}` · prompt `{summary.prompt_version}` · "
            f"input `{summary.input_hash}`.",
        ]
    )
    return "\n".join(lines).rstrip() + "\n"


def _plain_language_path(period: Period) -> str:
    if not period.relative_path.endswith(".md"):
        raise ValueError(f"rollup Markdown path lacks .md suffix: {period.relative_path}")
    return period.relative_path[:-3] + "-plain-language.md"


def _agent_markdown(
    team: TeamData,
    agent: Agent,
    name: AgentNameResult,
    phases: Sequence[PhaseWindow],
    summaries: Mapping[str, SummaryResult],
) -> str:
    lines = [
        f"# {name.short_name}",
        "",
        f"Team: `{team.team_slug}` · thread `{agent.thread_id}`  ",
        f"Official path: `{agent.agent_path}`  ",
        f"Coordinator nickname: `{agent.nickname or 'none'}`  ",
        f"Naming rationale: {name.rationale}",
        "",
        "## Lifetime summary",
        "",
        clean_summary_prose(name.lifetime_summary or "")
        or "Unavailable in this summary version.",
        "",
        "## Work phases",
        "",
    ]
    for phase in phases:
        result = summaries[phase.summary_key]
        lines.extend(
            [
                f"### {_local_time(phase.start_ms, team.display_timezone)} — {result.phrase}",
                "",
                result.paragraph,
                "",
            ]
        )
        lines.extend(
            f"- **{_local_time(item.at_ms, team.display_timezone)}** — {item.text}"
            for item in result.work_summary
        )
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def _time_range(team: TeamData, phases: Sequence[PhaseWindow]) -> tuple[int, int]:
    values: list[int] = []
    values.extend(phase.start_ms for phase in phases)
    values.extend(phase.end_ms for phase in phases)
    for agent in team.agents:
        values.append(agent.started_at_ms)
        if agent.ended_at_ms is not None:
            values.append(agent.ended_at_ms)
    for turn in team.turns:
        values.append(turn.started_at_ms)
        if turn.ended_at_ms is not None:
            values.append(turn.ended_at_ms)
    values.extend(event.timestamp_ms for event in team.events)
    for tool in team.tool_calls:
        values.append(tool.started_at_ms)
        if tool.ended_at_ms is not None:
            values.append(tool.ended_at_ms)
    values.extend(edge.timestamp_ms for edge in team.edges)
    if team.window_start_ms is not None:
        values.append(team.window_start_ms)
    if team.window_end_ms is not None:
        values.append(team.window_end_ms)
    if not values:
        return 0, 1000
    start = team.window_start_ms if team.window_start_ms is not None else min(values)
    end = team.window_end_ms if team.window_end_ms is not None else max(values)
    return start, max(start + 1000, end)


def _in_window(team: TeamData, timestamp_ms: int) -> bool:
    if team.window_start_ms is not None and timestamp_ms < team.window_start_ms:
        return False
    return team.window_end_ms is None or timestamp_ms < team.window_end_ms


def _summary_for_agent_at(
    agent_id: str,
    at_ms: int,
    phases_by_agent: Mapping[str, Sequence[PhaseWindow]],
    summaries: Mapping[str, SummaryResult],
) -> SummaryResult | None:
    own = phases_by_agent.get(agent_id, ())
    if not own:
        return None
    containing = [phase for phase in own if phase.start_ms <= at_ms <= phase.end_ms]
    selected = min(
        containing or own,
        key=lambda phase: (abs(phase.start_ms - at_ms), phase.start_ms),
    )
    return summaries.get(selected.summary_key)


def _edge_obj(
    edge: Edge,
    agents: Mapping[str, Agent],
    phases_by_agent: Mapping[str, Sequence[PhaseWindow]],
    summaries: Mapping[str, SummaryResult],
    names: Mapping[str, AgentNameResult],
) -> dict[str, object] | None:
    target = agents.get(edge.to_thread_id)
    if target is None or edge.from_thread_id not in agents:
        return None
    summary = _summary_for_agent_at(
        edge.to_thread_id, edge.timestamp_ms, phases_by_agent, summaries
    )
    readable_kind = "message" if edge.kind == "followup" else edge.kind
    action = {
        "spawn": "Spawn",
        "continuation": "Continue as",
        "message": "Message to",
        "interrupt": "Interrupt",
    }.get(readable_kind, readable_kind.replace("_", " ").title())
    phrase = f"{action} {_agent_name(target, names).short_name}"
    paragraph = (
        summary.paragraph
        if summary is not None and summary.summary_available
        else phrase
    )
    full_text = edge.message_text or ""
    status = ""
    if edge.content_availability == "encrypted":
        # Codex may leave a plaintext routing envelope next to an encrypted body. Do not
        # misrepresent that envelope as the user's full collaboration instruction.
        full_text = ""
        status = (
            "Codex persisted this collaboration payload encrypted. The exact plaintext is not "
            "available to an offline transcript parser; the paragraph is inferred from the "
            "receiving agent's work."
        )
    elif not full_text:
        status = "No plaintext message body was present in the source log."
    return {
        "id": edge.edge_id,
        "source_id": edge.from_thread_id,
        "target_id": edge.to_thread_id,
        "source_ms": edge.timestamp_ms,
        "target_ms": (
            target.started_at_ms
            if readable_kind in ("spawn", "continuation")
            else edge.timestamp_ms
        ),
        "kind": (
            readable_kind
            if readable_kind in ("spawn", "continuation", "message", "result")
            else "other"
        ),
        "phrase": phrase,
        "paragraph": paragraph,
        "full_text": full_text,
        "content_status": status,
    }


def _result_edge_objs(
    team: TeamData,
    agents: Mapping[str, Agent],
    phases_by_agent: Mapping[str, Sequence[PhaseWindow]],
    summaries: Mapping[str, SummaryResult],
    names: Mapping[str, AgentNameResult],
) -> list[dict[str, object]]:
    """Render turn responses as details and one structural join per agent lifetime."""

    result: list[dict[str, object]] = []
    continuation_targets = {
        edge.to_thread_id for edge in team.edges if edge.kind == "continuation"
    }
    finals_by_agent: dict[str, list[Event]] = {}
    for event in team.events:
        if (
            event.kind in ("assistant_message", "inter_agent_message")
            and event.phase == "final_answer"
            and event.text
        ):
            finals_by_agent.setdefault(event.thread_id, []).append(event)
    turns_by_key: dict[tuple[str, str], Turn] = {}
    for turn in team.turns:
        # Preserve the legacy linear lookup's first-match behavior for malformed or
        # pre-validation archives that repeat a provider turn identity.
        turns_by_key.setdefault((turn.thread_id, turn.turn_id), turn)
    triggers_by_target: dict[str, list[Edge]] = {}
    for edge in team.edges:
        if edge.kind in ("spawn", "followup"):
            triggers_by_target.setdefault(edge.to_thread_id, []).append(edge)
    for agent in team.agents:
        if (
            agent.thread_id not in agents
            or agent.parent_thread_id is None
            or agent.parent_thread_id not in agents
            or agent.thread_id in continuation_targets
        ):
            continue
        finals = finals_by_agent.get(agent.thread_id, ())
        # A reused agent can complete many turns during one lifetime. Those responses are
        # messages, not additional fork/join structure, and retain the coordinator that
        # initiated each turn as their destination.
        for final in sorted(finals, key=lambda event: (event.timestamp_ms, event.event_id)):
            if not _in_window(team, final.timestamp_ms):
                continue
            target_id = _result_target(
                agent,
                final,
                turns_by_key,
                triggers_by_target,
            )
            if target_id is None or target_id not in agents:
                continue
            summary = _summary_for_agent_at(
                agent.thread_id, final.timestamp_ms, phases_by_agent, summaries
            )
            result.append(
                {
                    "id": f"turn-result-{final.event_id}",
                    "source_id": agent.thread_id,
                    "target_id": target_id,
                    "source_ms": final.timestamp_ms,
                    "target_ms": final.timestamp_ms,
                    "kind": "message",
                    "phrase": f"{_agent_name(agent, names).short_name} reports progress",
                    "paragraph": (
                        summary.paragraph
                        if summary is not None and summary.summary_available
                        else (final.text or "")[:500]
                    ),
                    "full_text": final.text or "",
                    "content_status": "",
                }
            )

        # The structural return mirrors the one immutable parent->child spawn. Keep it
        # independent from turn delivery: a completed thread can be resumed by a coordinator
        # other than its lineage parent, while its lifetime still joins the parent that forked it.
        if agent.ended_at_ms is None:
            continue
        phase_end_ms = max(
            (
                phase.end_ms - 1
                for phase in phases_by_agent.get(agent.thread_id, ())
            ),
            default=agent.ended_at_ms,
        )
        # Provider lifecycle records can be second-granular while the final response retains
        # milliseconds. Match the end of the rendered lifetime rather than drawing the join a
        # few pixels inside its block.
        return_ms = max(agent.ended_at_ms, phase_end_ms)
        if not _in_window(team, return_ms):
            continue
        parent = agents[agent.parent_thread_id]
        summary = _summary_for_agent_at(
            agent.thread_id, return_ms, phases_by_agent, summaries
        )
        child_name = _agent_name(agent, names).short_name
        parent_name = _agent_name(parent, names).short_name
        phrase = (
            f"{child_name} returns to {parent_name}"
            if agent.status == "completed"
            else f"{child_name} lifetime ends at {parent_name} ({agent.status})"
        )
        result.append(
            {
                "id": f"result-{agent.thread_id}",
                "source_id": agent.thread_id,
                "target_id": agent.parent_thread_id,
                "source_ms": return_ms,
                "target_ms": return_ms,
                "kind": "result",
                "phrase": phrase,
                "paragraph": (
                    summary.paragraph
                    if summary is not None and summary.summary_available
                    else f"{child_name} ended with status {agent.status}."
                ),
                "full_text": "",
                "content_status": (
                    f"Structural lifetime join. Agent status: {agent.status}. Turn responses "
                    "are available as detailed message edges."
                ),
            }
        )
    return result


def _result_target(
    agent: Agent,
    final: Event,
    turns_by_key: Mapping[tuple[str, str], Turn],
    triggers_by_target: Mapping[str, Sequence[Edge]],
) -> str | None:
    """Resolve the coordinator that initiated the turn producing ``final``.

    ``parent_thread_id`` records immutable spawn lineage, not necessarily the coordinator that
    later reuses a completed agent thread. Codex records a resumed turn start at whole-second
    precision and its triggering ``followup_task`` activity at millisecond precision, so match
    within a small clock-resolution window. An unmatched result retains the lineage-parent
    fallback used by older archives.
    """

    turn = (
        turns_by_key.get((final.thread_id, final.turn_id))
        if final.turn_id is not None
        else None
    )
    if turn is None:
        return agent.parent_thread_id
    trigger_slop_ms = 2_000
    triggers = [
        edge
        for edge in triggers_by_target.get(agent.thread_id, ())
        if abs(edge.timestamp_ms - turn.started_at_ms) <= trigger_slop_ms
    ]
    if not triggers:
        return agent.parent_thread_id
    trigger = min(
        triggers,
        key=lambda edge: (
            abs(edge.timestamp_ms - turn.started_at_ms),
            edge.timestamp_ms,
            edge.edge_id,
        ),
    )
    return trigger.from_thread_id
def _event_objs(
    team: TeamData, visible_agent_ids: frozenset[str]
) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for event in team.events:
        if (
            event.thread_id in visible_agent_ids
            and _in_window(team, event.timestamp_ms)
            and event.kind
            in (
                "user_prompt",
                "assistant_message",
                "inter_agent_message",
                "external_message",
            )
        ):
            result.append(
                {"at_ms": event.timestamp_ms, "agent_id": event.thread_id, "kind": event.kind}
            )
    for tool in team.tool_calls:
        if tool.thread_id not in visible_agent_ids or not _in_window(
            team, tool.started_at_ms
        ):
            continue
        count = sum(value for _, value in tool.nested_tools) or 1
        result.extend(
            {"at_ms": tool.started_at_ms, "agent_id": tool.thread_id, "kind": "tool_call"}
            for _ in range(count)
        )
    result.sort(
        key=lambda item: (
            _object_int(item.get("at_ms")),
            _object_string(item.get("agent_id")),
            _object_string(item.get("kind")),
        )
    )
    return result


def _event_stats(
    events: Sequence[Mapping[str, object]], agent_count: int
) -> dict[str, int]:
    counts = {
        "user_prompts": 0,
        "agent_responses": 0,
        "inter_agent_messages": 0,
        "external_messages": 0,
        "tool_calls": 0,
    }
    labels = {
        "user_prompt": "user_prompts",
        "assistant_message": "agent_responses",
        "inter_agent_message": "inter_agent_messages",
        "external_message": "external_messages",
        "tool_call": "tool_calls",
    }
    for event in events:
        label = labels.get(_object_string(event.get("kind")))
        if label is not None:
            counts[label] += 1
    return {**counts, "active_agents": agent_count, "events": len(events)}


def _object_int(value: object) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


def _object_string(value: object) -> str:
    return value if isinstance(value, str) else ""


def _write_compressible_json(path: Path, value: JsonValue) -> int:
    return write_text_with_gzip_invalidation(
        path, canonical_json(value)
    )


def _write_gzip_only_json(path: Path, value: JsonValue) -> int:
    """Store a browser-facing document as ``<path>.gz`` with no identity twin.

    For the two families that dominate the archive's size and are only ever fetched by a browser
    -- the per-phase detail documents and the artifact catalogue -- the compressed member is the
    stored form. Both were previously kept twice: on the measured archive the identity copies came
    to 203.4 MiB of detail across 11,899 phases plus a 47.0 MiB catalogue, none of which was ever
    sent, because every client that fetches them announces gzip and gets the companion.

    The path recorded in the timeline stream stays ``.json``. That is the resource; ``.json.gz``
    is how it is stored, and the server presents one from the other. Keeping the logical name
    means ``app.js`` needs no notion of this at all.
    """

    return write_gzip_only(path, canonical_json(value))


def _standalone_server() -> str:
    return (files("wrkviz") / "standalone_server.py").read_text(
        encoding="utf-8"
    )


def standalone_query_source() -> str:
    """Return the dependency-free query CLI copied into every archive."""

    return (files("wrkviz") / "query.py").read_text(encoding="utf-8")


def archive_makefile() -> str:
    """Return discoverable wrappers for the archive's human-facing commands."""

    return (
        ".DEFAULT_GOAL := help\n"
        ".PHONY: help serve open run-stats query prompts\n"
        "PYTHON ?= python3\n"
        "PORT ?= 8765\n"
        "QUERY_ARGS ?= teams\n\n"
        "PROMPT_ARGS ?=\n\n"
        "help:\n"
        "\t@printf '%s\\n' \\\n"
        "\t  'Agent timeline archive commands:' \\\n"
        "\t  '  make serve                 Serve the website on PORT (default 8765)' \\\n"
        "\t  '  make open                  Serve it and ask Python to open a browser' \\\n"
        "\t  '  make run-stats             Print recorded pipeline runs and token costs' \\\n"
        "\t  '  make query QUERY_ARGS=...  Run the read-only ./timeline CLI' \\\n"
        "\t  '  make prompts PROMPT_ARGS=...  Print owner prompts' \\\n"
        "\t  '  ./timeline --help          Discover all query and search commands' \\\n"
        "\t  '' \\\n"
        "\t  'Variables: PORT, PYTHON, QUERY_ARGS, PROMPT_ARGS'\n\n"
        "serve:\n\t$(PYTHON) ./serve.py --port $(PORT)\n\n"
        "open:\n\t$(PYTHON) ./serve.py --port $(PORT) --open\n\n"
        "run-stats:\n\t$(PYTHON) ./run_stats.py\n\n"
        "query:\n\t@./timeline $(QUERY_ARGS)\n\n"
        "prompts:\n\t@./timeline prompts $(PROMPT_ARGS)\n"
    )


def archive_readme(
    heading: str,
    introduction: str,
    export_window_note: str,
) -> str:
    """Return the human guide installed at the root of a generated archive."""

    return (
        f"# {heading}\n\n"
        f"{introduction}\n\n"
        "## Start here\n\n"
        "```bash\n"
        "make help\n"
        "make serve\n"
        "# open http://127.0.0.1:8765/\n"
        "```\n\n"
        "Do not open `index.html` directly: browsers block its JSON requests from `file://`. "
        "Use `PORT=9000 make serve` to choose another port, or `make open` to ask Python "
        "to open the browser. The bundled server negotiates deterministic gzip sidecars and "
        "revalidates cached files.\n\n"
        "## Human-facing methods\n\n"
        "- `./timeline`: canonical, dependency-free, read-only CLI for teams, agents, phases, "
        "prompts, messages, full-text search, and stable references. Start with "
        "`./timeline --help`.\n"
        "- `./serve.py`: local static web server used by `make serve` and `make open`. Start "
        "with `./serve.py --help` when calling it directly.\n"
        "- `./run_stats.py`: prints pipeline-run status and recorded model-token costs; "
        "`make run-stats` is the convenient wrapper.\n"
        "- `Makefile`: discoverable wrappers for those methods. Plain `make` and `make help` "
        "print the command summary without starting a server.\n\n"
        "There is deliberately no second query launcher: `./timeline` is the single supported "
        "archive-local CLI.\n\n"
        "## Read-only query quickstart\n\n"
        "Prompt output defaults to readable text; supported formats are `json`, `jsonl`, "
        "`markdown`, and `text`. Copy a stable reference returned by a list command or "
        "`search` into `show`; references use `team:TEAM`, `agent:TEAM::ID`, "
        "`phase:TEAM::ID`, or `rollup:TEAM::KIND::START_MS`.\n\n"
        "```bash\n"
        "./timeline teams\n"
        "./timeline agents --team TEAM --format jsonl\n"
        "./timeline show agent:TEAM::AGENT_ID --format markdown\n"
        "./timeline show phase:TEAM::PHASE_ID --transcript --format markdown\n"
        "./timeline search \"SEARCH TEXT\" --in owner-prompts --limit 20\n"
        "./timeline search \"SEARCH TEXT\" --in agent-responses --limit 20\n"
        "./timeline search \"SEARCH TEXT\" --in all-transcript --limit 20\n"
        "./timeline prompts --range 200-300\n"
        "./timeline prompts --format jsonl > prompts.jsonl\n"
        "```\n\n"
        "When present, `extracted/transcripts/prompts.jsonl` is the complete mechanical prompt "
        "report and `messages.jsonl` adds mechanically associated responses.\n\n"
        "## Top-level map\n\n"
        "- `README.md`, `Makefile`, `timeline`, `serve.py`, and `run_stats.py` are the human "
        "entrypoints described above.\n"
        "- `index.html`, `app.js`, `timeline-core.js`, `style.css`, and `vendor/` are the "
        "generated offline website payload.\n"
        "- `manifest.json`, when present, is the latest pipeline-run status; `runs/` contains "
        "append-only per-run receipts and token accounting.\n"
        "- `data/` is the generated website/query projection; its `export.json` inventories "
        "presentation-owned files and the selected time window.\n"
        "- `extracted/` contains zero-token mechanical transcript projections when extraction "
        "has run.\n"
        "- `teams/` contains per-team normalized inputs, cached summaries, and published "
        "Markdown summaries.\n"
        "- `snapshot-root.json`, when present, records where this archive keeps the vendor "
        "source snapshots ingestion reads. They are input rather than output and are usually the "
        "largest thing involved, so they are kept outside this directory and are not served; "
        "back that store up alongside this archive if you intend to keep ingesting into it.\n"
        "- `qa/`, when present, contains human- or automation-produced validation evidence.\n"
        "- `.agent-team-timeline.json` marks this as a managed archive, "
        "`.agent-team-timeline.lock` serializes writers, and `.gitignore` identifies local-only "
        "state. None is a user command.\n\n"
        f"{export_window_note}\n"
    )


def _pull_request_references(
    text: str, metadata: Mapping[PullRequestKey, PullRequestMetadata]
) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for reference in find_pull_request_references(text):
        key = PullRequestKey(
            reference.link.repository.slug, reference.link.number
        )
        item: dict[str, object] = {
            "start": reference.start,
            "end": reference.end,
            "text": reference.text,
            "kind": reference.kind.value,
            "repository": reference.link.repository.slug,
            "number": reference.link.number,
            "url": reference.link.url,
        }
        pull = metadata.get(key)
        if pull is not None:
            item.update(
                {
                    "title": pull.title,
                    "state": pull.state,
                    "draft": pull.draft,
                    "merged_at": pull.merged_at,
                    "body_excerpt": pull.body_excerpt,
                    "base_ref": pull.base_ref,
                    "head_label": pull.head_label,
                    "author": pull.author,
                    "updated_at": pull.updated_at,
                }
            )
        result.append(item)
    return result


def _transcript_entry_obj(
    entry: TranscriptEntry, metadata: Mapping[PullRequestKey, PullRequestMetadata]
) -> dict[str, object]:
    result = entry.to_json_obj()
    result["pull_requests"] = _pull_request_references(entry.text, metadata)
    return result


def _generated_path(archive: Path, relative_path: str) -> Path:
    """Resolve one presentation path without crossing a symlink or archive boundary."""

    relative = Path(relative_path)
    if relative.is_absolute() or not relative.parts or any(
        part in ("", ".", "..") for part in relative.parts
    ):
        raise ValueError(f"unsafe generated presentation path: {relative_path!r}")
    root = archive.resolve()
    cursor = archive
    for part in relative.parts[:-1]:
        cursor /= part
        if cursor.is_symlink():
            raise ValueError(f"refusing generated presentation parent symlink: {cursor}")
    path = archive.joinpath(*relative.parts)
    try:
        path.parent.resolve().relative_to(root)
    except ValueError as error:
        raise ValueError(
            f"generated presentation path escapes archive: {relative_path}"
        ) from error
    if path.is_symlink():
        raise ValueError(f"refusing generated presentation symlink: {path}")
    return path


def _remove_generated_file(archive: Path, relative_path: str) -> int:
    """Remove one known presentation file without following a path outside *archive*."""

    path = _generated_path(archive, relative_path)
    sidecar = gzip_sidecar_path(path)
    if sidecar.is_symlink():
        raise ValueError(f"refusing symlinked gzip sidecar: {sidecar}")
    changed = 0
    if not path.exists():
        if sidecar.is_file():
            sidecar.unlink()
            return 1
        return 0
    if not path.is_file():
        raise ValueError(f"generated presentation path is not a file: {path}")
    path.unlink()
    changed += 1
    if sidecar.is_file():
        sidecar.unlink()
        changed += 1
    return changed


_PRESENTATION_MANIFEST = "data/export.json"
_PRESENTATION_COMMON = frozenset(
    {
        "index.html",
        "timeline-core.js",
        "app.js",
        "style.css",
        "serve.py",
        "run_stats.py",
        "timeline",
        "Makefile",
        "README.md",
        "vendor/README.md",
        "vendor/markdown-it-15.0.0.min.js",
        "vendor/markdown-it-LICENSE.txt",
    }
)
_PRESENTATION_RETIRED = frozenset({"query.py"})


def _safe_presentation_file(relative: str) -> str:
    # Schema 3 first, because the rule below -- "a trailing `.gz` names the compressed sidecar of
    # a plain file" -- is a convention of the older generations and schema 3 deliberately breaks
    # it: its shards ARE `.gz` and have no plain twin, which is the duplication it exists to
    # remove. Stripping the suffix here would ask whether `…/2026-05-04.jsonl` is a legal
    # presentation path, and it is not, because it is never written.
    if is_schema_3_path(relative):
        return relative
    base = relative.removesuffix(".gz")
    if relative.endswith(".gz") and base.endswith(".gz"):
        raise ValueError(f"nested presentation gzip sidecar: {relative!r}")
    path = Path(base)
    if path.is_absolute() or not path.parts or any(
        part in ("", ".", "..") for part in path.parts
    ):
        raise ValueError(f"unsafe presentation manifest path: {relative!r}")
    if base in _PRESENTATION_COMMON or base in _PRESENTATION_RETIRED or base in {
        _PRESENTATION_MANIFEST,
        "data/timeline.json",
        "data/timeline-v2.json",
        "data/timeline-v2/manifest.json",
        "data/artifacts.json",
    }:
        return relative
    if (
        len(path.parts) in (3, 4)
        and tuple(path.parts[:2]) == ("data", "details")
        and path.suffix == ".json"
    ):
        return relative
    if (
        len(path.parts) == 4
        and tuple(path.parts[:3]) == ("data", "timeline-v2", "objects")
        and path.suffix == ".json"
        and len(path.stem) == 64
        and all(character in "0123456789abcdef" for character in path.stem)
    ):
        return relative
    if (
        len(path.parts) >= 4
        and path.parts[0] == "teams"
        and path.parts[2] == "summaries"
        and path.suffix == ".md"
    ):
        return relative
    raise ValueError(f"unrecognized presentation manifest path: {relative!r}")


def _previous_presentation_files(archive: Path) -> set[str]:
    path = _generated_path(archive, _PRESENTATION_MANIFEST)
    if not path.is_file():
        return set()
    root = as_object(read_json(path), str(path))
    if root.get("schema_version") != 1 or root.get("kind") not in {
        "single-team-export",
        "multi-team-export",
    }:
        raise ValueError(f"unsupported presentation manifest at {path}")
    result: set[str] = set()
    for index, raw in enumerate(
        as_array(root.get("generated_files"), str(path) + ".generated_files")
    ):
        relative = as_string(raw, f"{path}.generated_files[{index}]")
        _safe_presentation_file(relative)
        result.add(relative)
    return result


def _remove_stale_presentation_files(
    archive: Path, previous: set[str], current: set[str]
) -> int:
    changed = 0
    for relative in sorted(previous - current):
        _safe_presentation_file(relative)
        path = _generated_path(archive, relative)
        if path.is_file():
            path.unlink()
            changed += 1
    return changed


#: The schema-1 monolith. Retired as an *output* -- see :func:`retired_generation_files` -- but
#: still the intermediate format the combined export merges, so the name stays a constant rather
#: than becoming a literal in two places that could drift apart.
SCHEMA_1_TIMELINE_PATH = "data/timeline.json"

#: The schema-2 presentation generation, retired the day `timeline_shards.SCHEMA_2_IS_PUBLISHED`
#: became ``False``. Two entries and a whole tree: the bootstrap, its gzip twin, and everything
#: under ``data/timeline-v2/`` -- the content-addressed objects, their gzip twins, and the
#: reachability manifest that names them.
#:
#: Taken wholesale rather than by object-name shape, matching `archive_gc._schema_2_generation_files`
#: and for the same reason it gives: that directory belongs to one format generation and to nothing
#: else. The two enumerations are deliberately not shared -- `archive_gc` imports nothing from
#: `render` and `render` imports nothing from `archive_gc`, because the collector must be able to
#: reason about an archive a *different* build produced -- so `test_timeline_archive_gc.py` asserts
#: they agree on a real archive instead.
SCHEMA_2_PRESENTATION_ROOT = "data/timeline-v2"


def _schema_2_presentation_files(archive: Path) -> set[str]:
    """Every schema-2 presentation path physically present under *archive*."""

    found: set[str] = set()
    for relative in (SCHEMA_2_BOOTSTRAP_PATH, SCHEMA_2_BOOTSTRAP_PATH + ".gz"):
        path = archive / Path(relative)
        if path.is_file() and not path.is_symlink():
            found.add(relative)
    root = archive / Path(SCHEMA_2_PRESENTATION_ROOT)
    if not root.is_dir() or root.is_symlink():
        return found
    # `os.walk` with `followlinks=False`, and symlinked files skipped, so "the generation's tree"
    # never means "wherever a link in it points". The same care `_safe_output_path` takes on the
    # writing side, for the same reason: this set decides what a build declines to delete, and a
    # path that leaves the archive is one it cannot make any promise about.
    for directory, _subdirectories, names in os.walk(root, followlinks=False):
        for name in names:
            path = Path(directory) / name
            if path.is_symlink() or not path.is_file():
                continue
            found.add(path.relative_to(archive).as_posix())
    return found


def retired_generation_files(archive: Path, current: set[str]) -> frozenset[str]:
    """The files of a retired generation that an older build left behind, and this build must
    **not** delete.

    Two generations are in this set now. A published export stopped writing ``data/timeline.json``
    and its ``.gz`` twin -- 280,971,189 bytes on the measured archive, of which the plain file
    alone is 246,973,399 -- and it has now stopped writing the schema-2 presentation generation as
    well, 1,503,831,881 bytes across 661 files, because `static/app.js` reads schema 3 and nothing
    is left that needs the older one. ``./timeline`` prefers schema 3, then schema 2, and opens the
    monolith only when neither is present; the browser bundle does the same, in the same order.

    **Not writing it and deleting it are different decisions, and this function is the seam.**
    Left to itself, `_remove_stale_presentation_files` would delete both generations the instant a
    newer tool rebuilt an older archive: it reaps ``previous - current``, they are in every older
    manifest's ``previous``, and they are in no new build's ``current``. That is 1.8 GB of
    irreversible unlink performed as a side effect of a build the operator asked for a different
    reason, and it is the exact shape of surprise the archive's own stale-file rules are careful
    about elsewhere -- `_finish_object_generation` held a retained set for one generation rather
    than trusting a single narrowed build, and `prune_retired_query_artifacts` refuses to remove a
    launcher whose bytes it does not recognise.

    So a retired generation is *retired*, not reaped: this build declines to rewrite it, records it
    in the export manifest's ``retired_files``, and leaves the bytes alone. Two things then remain
    true that would not be otherwise. An archive written by an older tool stays readable by an
    older reader -- an ``./timeline`` copy from before schema 3 existed, or a browser holding a
    cached ``app.js`` that has never heard of it, still finds what it looks for. And the deletion
    becomes something an operator asks for, with `gc`, which reports it by size first, moves it to
    a trash directory rather than unlinking it, and requires a second command to make that
    permanent.

    The schema-2 case is *why* that grace matters more than it did for the monolith, and the reason
    is `archive_gc._website_refusal`: a bundle is copied into the archive it was built with, so an
    archive built before this flip carries an ``app.js`` with no schema-3 mode. Deleting its
    schema-2 objects during an unrelated build would break the only surface that archive has, hours
    later, in somebody else's session. `gc` refuses to collect them until a rebuild has replaced
    the bundle -- and a rebuild is exactly the operation that reaches this function.

    *current* is the set this build did write, so that a mode which still writes schema 1 -- the
    combiner's intermediate -- never reports its own live output as retired.
    """

    retired: set[str] = set()
    for relative in (SCHEMA_1_TIMELINE_PATH, SCHEMA_1_TIMELINE_PATH + ".gz"):
        if relative in current:
            continue
        path = archive / Path(relative)
        if path.is_file() and not path.is_symlink():
            retired.add(relative)
    if not schema_2_is_published():
        retired.update(_schema_2_presentation_files(archive) - current)
    return frozenset(retired)


def prune_retired_query_artifacts(
    archive: Path,
    *,
    manifest_owned: bool = False,
) -> int:
    """Remove the retired generated ``query.py`` alias and only its bytecode cache.

    A manifest-owned launcher is already known to be presentation output. Without that
    ownership record, the launcher is removed only when its bytes still equal the bundled
    source. A modified or symlinked user file is left alone. The narrowly named bytecode cache
    is cleaned only for a managed archive or when this call recognized the generated launcher.
    """

    launcher = archive / "query.py"
    recognized_launcher = manifest_owned
    if launcher.exists() or launcher.is_symlink():
        if launcher.is_symlink() or not launcher.is_file():
            if manifest_owned:
                raise ValueError(f"retired generated query launcher is unsafe: {launcher}")
            return 0
        if manifest_owned or launcher.read_bytes() == standalone_query_source().encode("utf-8"):
            launcher.unlink()
            recognized_launcher = True
            changed = 1
        else:
            return 0
    else:
        changed = 0

    marker = archive / ".agent-team-timeline.json"
    if not recognized_launcher and (not marker.is_file() or marker.is_symlink()):
        return changed
    cache = archive / "__pycache__"
    if not cache.exists():
        return changed
    if cache.is_symlink() or not cache.is_dir():
        raise ValueError(f"retired query bytecode cache is unsafe: {cache}")
    for candidate in sorted(cache.glob("query.*.pyc")):
        if candidate.is_symlink() or not candidate.is_file():
            raise ValueError(f"retired query bytecode entry is unsafe: {candidate}")
        candidate.unlink()
        changed += 1
    try:
        cache.rmdir()
    except OSError:
        pass
    return changed


def render_archive(
    archive: Path,
    team: TeamData,
    phases: Sequence[PhaseWindow],
    phase_summaries: Mapping[str, SummaryResult],
    periods: Sequence[Period],
    rollup_summaries: Mapping[str, SummaryResult],
    plain_rollup_summaries: Mapping[str, SummaryResult],
    rollup_stats: Mapping[str, PhaseStats],
    glossary_terms: Sequence[GlossaryTerm],
    project_overview: SummaryResult,
    agent_names: Mapping[str, AgentNameResult],
    pull_request_metadata: Mapping[PullRequestKey, PullRequestMetadata],
    artifact_catalog: ArtifactCatalog,
    site_identity: SiteIdentity,
    *,
    _precompress: bool = True,
    _published: bool = True,
    _export_window: DateWindow | None = None,
) -> dict[str, int]:
    """Regenerate all presentation files without invoking a summary backend.

    ``_published`` distinguishes the two things this function is asked to produce, which used to
    be distinguished by a flag called ``_write_shards`` that said only half of it. A **published**
    render is an archive a human or a browser opens: it gets schema 2 and schema 3 and no
    schema-1 monolith. An **unpublished** render is the combined export's per-team intermediate,
    written into a temporary directory that `multi_team` reads back and deletes; it gets the
    schema-1 monolith and nothing else, because schema 1 is the format the combiner merges and
    the two shard generations would be built twice -- once per team here, once over the merged
    timeline there -- and thrown away.

    The flag has to be one flag rather than two. Letting "writes shards" and "writes the
    monolith" diverge would allow an archive with neither, which is an export no reader can
    open and no test can name.
    """

    previous_presentation_files = _previous_presentation_files(archive)
    changed = 0
    compressible_paths: set[str] = set()
    #: Relatives stored as ``<relative>.gz`` only. Disjoint from `compressible_paths`, which is
    #: the set that keeps an identity file *and* gains a companion.
    gzip_only_paths: set[str] = set()
    published_summary_paths: set[str] = set()
    phase_paths: dict[str, str] = {}
    agents_by_id = {agent.thread_id: agent for agent in team.agents}
    visible_agent_ids = phase_agent_ids(team, phases)
    phases_by_agent: dict[str, list[PhaseWindow]] = {}
    for phase in phases:
        phases_by_agent.setdefault(phase.agent_id, []).append(phase)
    for own_phases in phases_by_agent.values():
        own_phases.sort(key=lambda phase: (phase.start_ms, phase.phase_id))
    artifact_index = ArtifactRangeIndex.from_catalog(artifact_catalog)
    phase_artifact_ids: dict[str, tuple[str, ...]] = {}
    phase_output_artifact_ids: dict[str, tuple[str, ...]] = {}
    for phase in phases:
        summary = phase_summaries[phase.summary_key]
        agent = agents_by_id[phase.agent_id]
        phase_agent_name = _agent_name(agent, agent_names)
        raw_path = f"teams/{team.team_slug}/summaries/phases/{phase.phase_id}.md"
        detail_path = f"data/details/{phase.phase_id}.json"
        compressible_paths.add(raw_path)
        gzip_only_paths.add(detail_path)
        phase_paths[phase.phase_id] = detail_path
        artifact_ids = artifact_index.ids_for_range(
            phase.start_ms, phase.end_ms, phase.agent_id
        )
        output_artifact_ids = artifact_index.ids_for_range(
            phase.start_ms,
            phase.end_ms,
            phase.agent_id,
            outputs_only=True,
        )
        phase_artifact_ids[phase.phase_id] = artifact_ids
        phase_output_artifact_ids[phase.phase_id] = output_artifact_ids
        if summary.summary_available:
            changed += write_text_with_gzip_invalidation(
                _generated_path(archive, raw_path),
                _phase_markdown(team, agent, phase_agent_name, phase, summary),
            )
            published_summary_paths.add(raw_path)
        else:
            changed += _remove_generated_file(archive, raw_path)
        detail: dict[str, object] = {
            "phrase": summary.phrase,
            "paragraph": summary.paragraph,
            "summary_available": summary.summary_available,
            "stats": phase.stats.to_mapping(),
            "work_summary": [
                {
                    "at_ms": item.at_ms,
                    "text": item.text,
                    "pull_requests": _pull_request_references(
                        item.text, pull_request_metadata
                    ),
                }
                for item in summary.work_summary
            ],
            "transcript": [
                _transcript_entry_obj(entry, pull_request_metadata)
                for entry in phase.transcript
            ],
            "raw_summary_path": raw_path if summary.summary_available else "",
            "agent": _agent_identity_obj(agent, phase_agent_name),
            "artifact_ids": list(artifact_ids),
            "output_artifact_ids": list(output_artifact_ids),
        }
        changed += _write_gzip_only_json(
            _generated_path(archive, detail_path), narrow_json(detail, detail_path)
        )

    for agent in team.agents:
        own = sorted(phases_by_agent.get(agent.thread_id, []), key=lambda phase: phase.start_ms)
        agent_path = f"teams/{team.team_slug}/summaries/agents/{agent.thread_id}.md"
        compressible_paths.add(agent_path)
        if not own:
            changed += _remove_generated_file(archive, agent_path)
            continue
        name = _agent_name(agent, agent_names)
        if _agent_summary_available(name):
            changed += write_text_with_gzip_invalidation(
                _generated_path(archive, agent_path),
                _agent_markdown(
                    team,
                    agent,
                    name,
                    own,
                    phase_summaries,
                ),
            )
            published_summary_paths.add(agent_path)
        else:
            changed += _remove_generated_file(archive, agent_path)

    rollup_paths: dict[str, tuple[str, str]] = {}
    for period in periods:
        period_key = period.key + ":" + period.kind
        summary = rollup_summaries[period_key]
        plain_summary = plain_rollup_summaries[period_key]
        stats = rollup_stats[period_key]
        technical_path = period.relative_path if summary.summary_available else ""
        plain_path = (
            _plain_language_path(period) if plain_summary.summary_available else ""
        )
        rollup_paths[period_key] = (technical_path, plain_path)
        if technical_path:
            changed += write_text_with_gzip_invalidation(
                _generated_path(archive, technical_path),
                _rollup_markdown(team, period, summary, stats, "technical"),
            )
            published_summary_paths.add(technical_path)
        else:
            changed += _remove_generated_file(archive, period.relative_path)
        expected_plain_path = _plain_language_path(period)
        compressible_paths.update((period.relative_path, expected_plain_path))
        if plain_path:
            changed += write_text_with_gzip_invalidation(
                _generated_path(archive, plain_path),
                _rollup_markdown(
                    team, period, plain_summary, stats, "plain-language"
                ),
            )
            published_summary_paths.add(plain_path)
        else:
            changed += _remove_generated_file(archive, expected_plain_path)

    weeks = sorted({term.week for term in glossary_terms})
    glossary_root = _generated_path(
        archive, f"teams/{team.team_slug}/summaries/glossary"
    )
    if glossary_root.is_symlink():
        raise ValueError(f"refusing symlinked generated glossary directory: {glossary_root}")
    expected_week_paths: set[Path] = set()
    for week in weeks:
        year = week.split("-W", 1)[0]
        path = f"teams/{team.team_slug}/summaries/glossary/{year}/{week}-{team.team_slug}-glossary.md"
        week_path = _generated_path(archive, path)
        compressible_paths.add(path)
        if week_path.parent.is_symlink() or week_path.is_symlink():
            raise ValueError(f"refusing symlinked generated glossary path: {week_path}")
        expected_week_paths.add(week_path.resolve())
        changed += write_text_with_gzip_invalidation(
            _generated_path(archive, path),
            glossary_markdown(team.team_slug, week, glossary_terms),
        )
        published_summary_paths.add(path)
    if glossary_root.is_dir():
        for child in sorted(glossary_root.iterdir()):
            if child.is_symlink():
                raise ValueError(
                    f"refusing symlink in generated glossary directory: {child}"
                )
        for stale_path in sorted(glossary_root.glob("*/*.md")):
            year = stale_path.parent.name
            prefix = f"{year}-W"
            suffix = f"-{team.team_slug}-glossary.md"
            week_number = stale_path.name.removeprefix(prefix).removesuffix(suffix)
            owned_name = (
                len(year) == 4
                and year.isdigit()
                and stale_path.name.startswith(prefix)
                and stale_path.name.endswith(suffix)
                and len(week_number) == 2
                and week_number.isdigit()
            )
            if not owned_name or stale_path.resolve() in expected_week_paths:
                continue
            if stale_path.parent.is_symlink() or stale_path.is_symlink():
                raise ValueError(f"refusing symlinked generated glossary path: {stale_path}")
            if stale_path.is_file():
                stale_sidecar = gzip_sidecar_path(stale_path)
                if stale_sidecar.is_symlink():
                    raise ValueError(f"refusing symlinked gzip sidecar: {stale_sidecar}")
                stale_path.unlink()
                changed += 1
                if stale_sidecar.is_file():
                    stale_sidecar.unlink()
                    changed += 1
    expected_glossary_catalog_path = (
        f"teams/{team.team_slug}/summaries/glossary/{team.team_slug}-glossary.md"
    )
    compressible_paths.add(expected_glossary_catalog_path)
    glossary_catalog_path = ""
    if glossary_terms or project_overview.summary_available:
        glossary_catalog_path = expected_glossary_catalog_path
        changed += write_text_with_gzip_invalidation(
            _generated_path(archive, glossary_catalog_path),
            glossary_catalog_markdown(
                team.team_slug, glossary_terms, project_overview.paragraph
            ),
        )
        published_summary_paths.add(glossary_catalog_path)
    else:
        changed += _remove_generated_file(archive, expected_glossary_catalog_path)

    static_root = files("wrkviz") / "static"
    for asset_name in ("index.html", "timeline-core.js", "app.js", "style.css"):
        text = (static_root / asset_name).read_text(encoding="utf-8")
        changed += write_text_with_gzip_invalidation(
            _generated_path(archive, asset_name), text
        )
        compressible_paths.add(asset_name)
    vendor_root = static_root / "vendor"
    for vendor_name in (
        "README.md",
        "markdown-it-15.0.0.min.js",
        "markdown-it-LICENSE.txt",
    ):
        text = (vendor_root / vendor_name).read_text(encoding="utf-8")
        if vendor_name.endswith(".js"):
            changed += write_text_with_gzip_invalidation(
                _generated_path(archive, f"vendor/{vendor_name}"), text
            )
            compressible_paths.add(f"vendor/{vendor_name}")
        else:
            changed += int(
                write_text_if_changed(
                    _generated_path(archive, f"vendor/{vendor_name}"), text
                )
            )
    changed += int(
        write_text_if_changed(
            _generated_path(archive, "serve.py"),
            _standalone_server(),
            executable=True,
        )
    )
    run_stats_source = (files("wrkviz") / "run_stats.py").read_text(
        encoding="utf-8"
    )
    changed += int(
        write_text_if_changed(
            _generated_path(archive, "run_stats.py"),
            run_stats_source,
            executable=True,
        )
    )
    changed += int(
        write_text_if_changed(
            _generated_path(archive, "timeline"),
            standalone_query_source(),
            executable=True,
        )
    )
    changed += int(
        write_text_if_changed(
            _generated_path(archive, "Makefile"),
            archive_makefile(),
        )
    )
    changed += int(
        write_text_if_changed(
            _generated_path(archive, "README.md"),
            archive_readme(
                f"{team.team_slug} agent-team timeline",
                "This directory is a self-contained, version-controllable timeline archive.",
                "For an exported package, the requested slice is recorded in "
                "`data/export.json` under `display_window`; `./timeline` reports the actual "
                "team and record intervals. Do not infer the slice from file modification "
                "times.",
            ),
        )
    )

    summary_files: list[dict[str, object]] = []
    for relative in sorted(published_summary_paths):
        summary_path_file = _generated_path(archive, relative)
        if not summary_path_file.is_file() or summary_path_file.is_symlink():
            raise ValueError(
                f"published summary path is missing or unsafe: {summary_path_file}"
            )
        parts = summary_path_file.relative_to(
            _generated_path(archive, f"teams/{team.team_slug}/summaries")
        ).parts
        kind = parts[0] if parts else "summary"
        summary_files.append(
            {
                "label": summary_path_file.stem.replace("-", " "),
                "kind": kind,
                "period": summary_path_file.stem,
                "path": relative,
            }
        )

    start_ms, end_ms = _time_range(team, phases)
    latest_ms = max(
        (
            event.timestamp_ms
            for event in team.events
            if event.thread_id in visible_agent_ids
            and _in_window(team, event.timestamp_ms)
        ),
        default=end_ms,
    )
    agent_objs: list[dict[str, object]] = []
    for agent in team.agents:
        if agent.thread_id not in visible_agent_ids:
            continue
        own = phases_by_agent.get(agent.thread_id, [])
        own_end = max((phase.end_ms for phase in own), default=agent.ended_at_ms or end_ms)
        track_name = _agent_name(agent, agent_names)
        track_start = max(agent.started_at_ms, start_ms)
        track_end = min(max(agent.ended_at_ms or own_end, own_end), end_ms)
        track_end = max(track_start + 1000, track_end)
        agent_objs.append(
            {
                "id": agent.thread_id,
                "team": team.team_slug,
                "parent_id": agent.parent_thread_id,
                "path": agent.agent_path,
                "label": track_name.short_name,
                "short_name": track_name.short_name,
                "official_name": agent.agent_path,
                "official_leaf": _official_leaf(agent),
                "nickname": agent.nickname or "",
                "naming_rationale": track_name.rationale,
                "lifetime_summary": clean_summary_prose(
                    track_name.lifetime_summary or ""
                ),
                "summary_available": _agent_summary_available(track_name),
                "depth": agent.depth,
                "start_ms": track_start,
                "end_ms": min(track_end, end_ms),
                "status": agent.status,
                "artifact_ids": list(
                    artifact_index.ids_for_range(
                        track_start, track_end, agent.thread_id
                    )
                ),
                "output_artifact_ids": list(
                    artifact_index.ids_for_range(
                        track_start,
                        track_end,
                        agent.thread_id,
                        outputs_only=True,
                    )
                ),
            }
        )
    phase_objs = [
        {
            "id": phase.phase_id,
            "agent_id": phase.agent_id,
            "start_ms": phase.start_ms,
            "end_ms": phase.end_ms,
            "phrase": phase_summaries[phase.summary_key].phrase,
            "paragraph": phase_summaries[phase.summary_key].paragraph,
            "summary_available": phase_summaries[
                phase.summary_key
            ].summary_available,
            "detail_path": phase_paths[phase.phase_id],
            "stats": phase.stats.to_mapping(),
            "states": [state.to_json_obj() for state in phase.states],
            "artifact_ids": list(phase_artifact_ids[phase.phase_id]),
            "output_artifact_ids": list(
                phase_output_artifact_ids[phase.phase_id]
            ),
        }
        for phase in phases
    ]
    visible_agents = {
        agent_id: agent
        for agent_id, agent in agents_by_id.items()
        if agent_id in visible_agent_ids
    }
    edge_objs = [
        value
        for edge in team.edges
        if edge.from_thread_id in visible_agent_ids
        and edge.to_thread_id in visible_agent_ids
        and _in_window(team, edge.timestamp_ms)
        for value in [
            _edge_obj(
                edge,
                visible_agents,
                phases_by_agent,
                phase_summaries,
                agent_names,
            )
        ]
        if value is not None
    ]
    edge_objs.extend(
        _result_edge_objs(
            team,
            visible_agents,
            phases_by_agent,
            phase_summaries,
            agent_names,
        )
    )
    edge_objs.sort(
        key=lambda item: (
            _object_int(item.get("source_ms")),
            _object_string(item.get("id")),
        )
    )
    rollup_objs: list[dict[str, object]] = []
    for period in periods:
        period_key = period.key + ":" + period.kind
        technical_path, plain_path = rollup_paths[period_key]
        technical_available = rollup_summaries[period_key].summary_available
        plain_available = plain_rollup_summaries[period_key].summary_available
        rollup_objs.append(
            {
                "kind": period.kind,
                "label": period.label + (" · partial" if period.partial else ""),
                "start_ms": period.start_ms,
                "end_ms": period.end_ms,
                # ``path`` remains the legacy stable selection identity. New readers use the
                # audience-specific paths only when their availability flags are true.
                "path": period.relative_path,
                "technical_path": technical_path,
                "plain_language_path": plain_path,
                "technical_summary_available": technical_available,
                "plain_language_summary_available": plain_available,
                "summary_available": technical_available or plain_available,
                "stats": rollup_stats[period_key].to_mapping(),
                "artifact_ids": list(
                    artifact_index.ids_for_range(
                        period.start_ms, period.end_ms
                    )
                ),
                "output_artifact_ids": list(
                    artifact_index.ids_for_range(
                        period.start_ms,
                        period.end_ms,
                        outputs_only=True,
                    )
                ),
            }
        )
    glossary_objs = [
        {
            "id": term.term_id,
            "term": term.term,
            "introduced_at_ms": term.introduced_at_ms,
            "available_at_ms": term.summary_available_at_ms,
            "occurrences": term.occurrences,
            "context": term.context,
            "definition": term.definition,
            "definition_status": term.definition_status,
            "week": term.week,
            "url": f"#glossary/{term.term_id}",
        }
        for term in glossary_terms
    ]
    activity_bins = build_activity_bins(
        team.team_slug,
        team.root_thread_id,
        phases,
        display_timezone=team.display_timezone,
        observed_start_ms=start_ms,
        observed_end_ms=end_ms,
    )
    event_objs = _event_objs(team, visible_agent_ids)
    search_records = build_search_records(
        team,
        visible_agent_ids,
        start_ms=start_ms,
        end_ms=end_ms,
    )
    timeline_stats = _event_stats(event_objs, len(visible_agent_ids))
    timeline: dict[str, object] = {
        "schema_version": 1,
        "generated_at": _iso(latest_ms),
        "source_digest": source_digest(team),
        "display_timezone": team.display_timezone,
        "display_timezone_source": site_identity.display_timezone_source,
        "range": {"start_ms": start_ms, "end_ms": end_ms},
        "teams": [
            {
                "slug": team.team_slug,
                "label": team.team_slug,
                "projects": [item.to_json_obj() for item in site_identity.projects],
                "hosts": [item.to_json_obj() for item in site_identity.hosts],
                "stats": timeline_stats,
            }
        ],
        "agents": agent_objs,
        "phases": phase_objs,
        "activity_bins": [item.to_json_obj() for item in activity_bins],
        "edges": edge_objs,
        "events": event_objs,
        "rollups": rollup_objs,
        "glossary": glossary_objs,
        "glossary_path": glossary_catalog_path,
        "project_overview": {
            "text": project_overview.paragraph,
            "summary_available": project_overview.summary_available,
            "evidence_status": (
                "supported"
                if project_overview.phrase == "Project overview supported"
                else "insufficient-evidence"
            ),
            "model": project_overview.model,
            "prompt_version": project_overview.prompt_version,
            "input_hash": project_overview.input_hash,
        },
        "summary_files": summary_files,
        "artifact_catalog_path": "data/artifacts.json",
        "projects": [project.to_json_obj() for project in artifact_catalog.projects],
        "stats": timeline_stats,
    }
    changed += _write_gzip_only_json(
        _generated_path(archive, "data/artifacts.json"),
        narrow_json(artifact_catalog.to_json_obj(), "artifact catalog"),
    )
    gzip_only_paths.add("data/artifacts.json")
    timeline_json = narrow_json(timeline)
    if not isinstance(timeline_json, dict):
        raise AssertionError("timeline projection must be an object")
    if not _published:
        # The combiner's intermediate. Schema 1 is written here and only here, because it is
        # this function's *output format* in that mode -- `multi_team` reads it straight back
        # out of the temporary directory -- rather than an artefact anybody keeps.
        changed += _write_compressible_json(
            _generated_path(archive, SCHEMA_1_TIMELINE_PATH), timeline_json
        )
        compressible_paths.add(SCHEMA_1_TIMELINE_PATH)
    if _precompress:
        for relative in sorted(compressible_paths):
            changed += int(sync_gzip_sidecar(_generated_path(archive, relative)))
    # Schema 3 and nothing else. `write_timeline_shards` used to run here too, and stopped the
    # day `static/app.js` learned to read schema 3 -- item 2 of the checklist on
    # `timeline_shards.SCHEMA_2_IS_PUBLISHED`. The search records go to the generation that
    # survives, which is why the corpus is not lost with the presentation half.
    v3_report = (
        write_timeline_v3(archive, timeline_json, search_records=search_records)
        if _published
        else None
    )
    if v3_report is not None:
        changed += v3_report.files_changed
    generated_files = set(_PRESENTATION_COMMON)
    generated_files.update(published_summary_paths)
    generated_files.update({_PRESENTATION_MANIFEST})
    if not _published:
        generated_files.add(SCHEMA_1_TIMELINE_PATH)
    if v3_report is not None:
        generated_files.update(v3_report.generated_files)
    for relative in sorted(compressible_paths):
        if gzip_sidecar_path(_generated_path(archive, relative)).is_file():
            generated_files.add(relative + ".gz")
    # A gzip-only relative is named in the manifest by the file that *exists*. Naming the plain
    # relative would make stale-file removal look for a file this build deliberately did not
    # write, and leave the `.gz` unnamed -- which is the shape that gets swept as an orphan.
    for relative in sorted(gzip_only_paths):
        generated_files.add(relative + ".gz")
    retired_files = (
        retired_generation_files(archive, generated_files) if _published else frozenset()
    )
    for relative in generated_files | retired_files:
        _safe_presentation_file(relative)
    retired_query_was_managed = "query.py" in previous_presentation_files
    changed += _remove_stale_presentation_files(
        archive, previous_presentation_files - retired_files, generated_files
    )
    changed += prune_retired_query_artifacts(
        archive,
        manifest_owned=retired_query_was_managed,
    )
    generated_values: list[JsonValue] = []
    for relative in sorted(generated_files):
        generated_values.append(relative)
    export_manifest: dict[str, JsonValue] = {
        "schema_version": 1,
        "kind": "single-team-export",
        "teams": [team.team_slug],
        "display_timezone": team.display_timezone,
        "display_window": (
            narrow_json(_export_window.to_json_obj())
            if _export_window is not None
            else None
        ),
        "source_digest": timeline_json["source_digest"],
        "generated_files": generated_values,
        "retired_files": [value for value in sorted(retired_files)],
    }
    changed += int(
        write_text_if_changed(
            _generated_path(archive, _PRESENTATION_MANIFEST),
            canonical_json(export_manifest),
        )
    )
    return {
        "files_changed": changed,
        "phases": len(phases),
        "activity_bins": len(activity_bins),
        "agents": len(visible_agent_ids),
        "edges": len(edge_objs),
        "rollups": len(periods),
        "summary_files": len(summary_files),
        "artifacts": len(artifact_catalog.artifacts),
        "projects": len(artifact_catalog.projects),
        # These six kept their names and changed the generation they describe, because what an
        # operator reads them for -- how many pieces is my timeline in, how big is the file a
        # browser must have before it can draw -- is unchanged. `shard_transfer_bytes` now equals
        # `shard_object_bytes` and that is the headline rather than a rounding error: a schema-3
        # shard is stored compressed and served as it is stored, so there is no second copy and no
        # gap between what is on the disk and what goes over the wire. `bootstrap_transfer_bytes`
        # equals `bootstrap_bytes` for the opposite reason -- the entry point has no gzip twin at
        # all -- and is 168,703 against schema 2's 5,702,530 on the measured archive. (It was
        # 89,298 before the three transcript search streams added their 96 catalogue entries; see
        # the "The search streams" section of `timeline_v3` for that 1.9x and why it was paid.)
        "detail_shards": v3_report.timeline_shards if v3_report is not None else 0,
        "search_records": len(search_records),
        "search_shards": v3_report.search_shards if v3_report is not None else 0,
        "bootstrap_bytes": v3_report.bootstrap_bytes if v3_report is not None else 0,
        "bootstrap_transfer_bytes": (
            v3_report.bootstrap_bytes if v3_report is not None else 0
        ),
        "shard_object_bytes": (
            v3_report.compressed_bytes + v3_report.index_bytes if v3_report is not None else 0
        ),
        "shard_transfer_bytes": (
            v3_report.compressed_bytes + v3_report.index_bytes if v3_report is not None else 0
        ),
    }


__all__ = ["render_archive"]

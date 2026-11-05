"""Deterministic synthetic transcripts, for testing rendering at a realistic size.

The committed browser fixture is four agents over one afternoon. At that size every level of
detail draws everything, so the level-of-detail machinery -- the part that keeps a large archive
responsive -- is never exercised by an automated check. This module closes that gap: it writes
Claude-shaped coordinator and subagent transcripts at a requested size, which the ordinary ingest,
summarize and build path turns into a real archive.

What it emits is structurally, not textually, realistic. Coordinators spawn subagents in
overlapping waves, subagents spawn their own, lifetimes run from twenty minutes to several hours
and overlap each other, work stops overnight so the aggregate view has real gaps, tool calls
arrive in bursts separated by idle stretches, and messages travel in both directions between a
parent and its children. The prose inside those records is drawn from small fixed vocabularies,
because the sentences are not what is being tested and inventing plausible ones would only make
the corpus larger.

Everything is a pure function of ``seed`` and the requested size. Two runs with the same
:class:`SyntheticScale` write byte-identical files, which is what lets the small size be used as a
regression baseline rather than merely as a load.

Sizes are named by :data:`PRESETS`. ``ci`` is small enough to generate and build in seconds and
long enough -- eleven days -- that a fitted view is well past the aggregate threshold. ``large``
and ``archive`` are for manual work at scale; ``archive`` is calibrated against a measured real
corpus of roughly 2,656 agents, 13,760 phases and 264,719 tool calls over 37 days.

Run it as a module::

    python3 -m wrkviz.synthetic --out /tmp/synth --preset ci --build

which writes transcripts under ``<out>/sources`` and, with ``--build``, an archive under
``<out>/archive`` that can be served with the ``serve.py`` inside it. It prints a JSON report of
what it produced.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import shutil
import sys
from collections.abc import Sequence
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path

from wrkviz.archive import JsonValue, write_text_if_changed


_MINUTE_MS = 60_000
_HOUR_MS = 60 * _MINUTE_MS
_DAY_MS = 24 * _HOUR_MS

#: The instant the first synthetic working day starts, as a UTC millisecond. A fixed epoch rather
#: than "now", because a fixture whose timestamps move cannot be a baseline.
DEFAULT_START_MS = 1_772_456_400_000  # 2026-03-02T13:00:00Z

#: How long each working day lasts, starting at the corpus's start instant. Nothing is emitted
#: outside it, which is what gives the zoomed-out view its overnight gaps -- and gaps are drawn by
#: leaving a hole, so a corpus that ran around the clock would never show one.
_WORK_WINDOW_MS = 10 * _HOUR_MS

_TOOL_NAMES = (
    "Bash",
    "Read",
    "Edit",
    "Grep",
    "Glob",
    "Write",
    "WebFetch",
)

_TOPICS = (
    "the retry budget",
    "the shard index",
    "the cache eviction path",
    "the ingest window",
    "the packaging manifest",
    "the timeout ladder",
    "the byte-range reader",
    "the admission gate",
    "the rollup boundary",
    "the identity resolver",
)

_ACTIONS = (
    "audit",
    "repair",
    "measure",
    "document",
    "harden",
    "reconcile",
)

_OBSERVATIONS = (
    "The failing case reproduces on the first attempt.",
    "The boundary is off by one whole window.",
    "Two callers disagree about which side is inclusive.",
    "The measurement is dominated by the first read.",
    "The check passes locally and fails under the resource box.",
    "The stale entry is never evicted because its key changes.",
)


@dataclass(frozen=True)
class SyntheticScale:
    """How much synthetic material to write, and with which seed.

    ``agents`` counts worker agents per team; each team also has one coordinator, so a team holds
    ``agents + 1`` threads. ``days`` is the number of working days the corpus spans, which is what
    decides whether a fitted view lands in the aggregate level of detail: roughly five days or
    more, on an ordinary window, does.
    """

    teams: int = 1
    agents: int = 200
    days: int = 11
    tool_calls_per_agent: int = 30
    seed: int = 20_260_826
    start_ms: int = DEFAULT_START_MS

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the requested size as a JSON object."""

        return {
            "teams": self.teams,
            "agents": self.agents,
            "days": self.days,
            "tool_calls_per_agent": self.tool_calls_per_agent,
            "seed": self.seed,
            "start_ms": self.start_ms,
        }


#: Named sizes. ``ci`` is the one a checked-in test may use; the other two are for manual work and
#: must not be committed as generated output.
PRESETS: dict[str, SyntheticScale] = {
    # ELEVEN days, not ten. The span has to be several days for a fitted view to be past the
    # aggregate threshold at all, and eleven puts the MIDDLE of the corpus inside a working window
    # rather than in the overnight gap -- so a check that zooms toward the centre finds work there
    # instead of measuring an empty night and calling it a rendering test.
    "ci": SyntheticScale(teams=1, agents=200, days=11, tool_calls_per_agent=30),
    "large": SyntheticScale(teams=2, agents=600, days=21, tool_calls_per_agent=60),
    # Calibrated against a measured real corpus -- 2,656 agents, 13,760 phases and 264,719 tool
    # calls over 37 days -- and measured against it: this size builds 2,643 agents, 16,527 phases
    # and 258,730 tool calls across three teams, in about half an hour, into 1.2 GB.
    "archive": SyntheticScale(teams=3, agents=880, days=37, tool_calls_per_agent=100),
}


@dataclass(frozen=True)
class SyntheticTeam:
    """One generated team: where its root transcript is, and what is inside it."""

    slug: str
    root_thread_id: str
    session_file: Path
    agents: int
    tool_calls: int
    records: int
    start_ms: int
    end_ms: int

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return this team's generated counts as a JSON object."""

        return {
            "slug": self.slug,
            "root_thread_id": self.root_thread_id,
            "session_file": str(self.session_file),
            "agents": self.agents,
            "tool_calls": self.tool_calls,
            "records": self.records,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
        }


@dataclass(frozen=True)
class SyntheticCorpus:
    """Every team written by one generation run, and the size that produced them."""

    scale: SyntheticScale
    root: Path
    teams: tuple[SyntheticTeam, ...]

    @property
    def span_ms(self) -> int:
        """Return the wall-clock interval the whole corpus covers."""

        return max(team.end_ms for team in self.teams) - min(
            team.start_ms for team in self.teams
        )

    def to_json_obj(self) -> dict[str, JsonValue]:
        """Return the whole run as a JSON object."""

        return {
            "scale": self.scale.to_json_obj(),
            "root": str(self.root),
            "span_ms": self.span_ms,
            "teams": [team.to_json_obj() for team in self.teams],
        }


def _iso(at_ms: int) -> str:
    moment = datetime.fromtimestamp(at_ms / 1000, tz=timezone.utc)
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{at_ms % 1000:03d}Z"


def _thread_uuid(seed: int, team_index: int) -> str:
    """Return a stable session identifier shaped like the one a session file is named after."""

    digest = hashlib.sha256(f"wrkviz-synthetic:{seed}:{team_index}".encode("utf-8")).hexdigest()
    return "-".join(
        (digest[0:8], digest[8:12], "4" + digest[13:16], "8" + digest[17:20], digest[20:32])
    )


@dataclass
class _Thread:
    """Records accumulating for one transcript file, before they are sorted and written."""

    thread_id: str
    records: list[tuple[int, int, dict[str, JsonValue]]]
    tools: int = 0

    def add(self, at_ms: int, kind: str, record: dict[str, JsonValue]) -> None:
        """Append one record with a per-thread unique identity, keyed for a stable sort."""

        sequence = len(self.records)
        record["uuid"] = f"{self.thread_id}-{kind}-{sequence}"
        record["timestamp"] = _iso(at_ms)
        self.records.append((at_ms, sequence, record))


@dataclass(frozen=True)
class _Worker:
    """One planned subagent: who spawned it, when, and how long it runs."""

    thread_id: str
    parent_id: str
    depth: int
    spawn_tool_id: str
    description: str
    agent_type: str
    start_ms: int
    end_ms: int


def _sentence(rng: random.Random) -> str:
    return (
        f"{rng.choice(_ACTIONS).capitalize()} {rng.choice(_TOPICS)}: "
        f"{rng.choice(_OBSERVATIONS)}"
    )


def _tool_input(rng: random.Random, name: str) -> dict[str, JsonValue]:
    topic = rng.choice(_TOPICS).replace("the ", "").replace(" ", "_")
    if name == "Bash":
        return {"command": f"make check TARGET={topic}"}
    if name in ("Read", "Write", "Edit"):
        return {"file_path": f"/work/project/{topic}.py"}
    if name in ("Grep", "Glob"):
        return {"pattern": topic, "path": "/work/project"}
    return {"url": f"https://example.invalid/{topic}"}


def _tool_output(rng: random.Random, name: str) -> str:
    return f"{name} finished. {rng.choice(_OBSERVATIONS)}"


def _plan_workers(scale: SyntheticScale, root_id: str, rng: random.Random) -> tuple[_Worker, ...]:
    """Lay out subagent lifetimes so that they overlap, nest, and span working days.

    Every fourth agent leads: it hangs off the coordinator and lives for hours. The rest attach to
    a lead that is alive when they start, and a few of those attach to a worker rather than a lead,
    which is what produces the depth-3 threads a real archive has.

    Every third lead runs long enough to cross midnight. Without those, every instant outside the
    daily working window would be empty, and a reader who zoomed into one would be measuring a
    blank page rather than the rendering path.
    """

    workers: list[_Worker] = []
    per_day = max(1, scale.agents // scale.days)
    for index in range(scale.agents):
        day = min(scale.days - 1, index // per_day)
        day_start = scale.start_ms + day * _DAY_MS
        offset = rng.randrange(0, _WORK_WINDOW_MS - 20 * _MINUTE_MS, _MINUTE_MS)
        start_ms = day_start + offset
        is_lead = index % 4 == 0
        long_haul = is_lead and index % 12 == 0
        if long_haul:
            end_ms = start_ms + rng.randrange(6 * 60, 15 * 60) * _MINUTE_MS
        elif is_lead:
            duration = rng.randrange(90, 300) * _MINUTE_MS
            end_ms = min(start_ms + duration, day_start + _WORK_WINDOW_MS + _HOUR_MS)
        else:
            duration = rng.randrange(20, 150) * _MINUTE_MS
            end_ms = min(start_ms + duration, day_start + _WORK_WINDOW_MS + _HOUR_MS)
        if end_ms - start_ms < 15 * _MINUTE_MS:
            end_ms = start_ms + 15 * _MINUTE_MS
        parent_id = root_id
        depth = 1
        if not is_lead:
            alive = [
                candidate
                for candidate in workers
                if candidate.start_ms < start_ms < candidate.end_ms
                and candidate.depth < 3
            ]
            leads = [candidate for candidate in alive if candidate.depth == 1]
            pool = alive if leads and rng.random() < 0.2 else leads
            if pool:
                chosen = pool[rng.randrange(len(pool))]
                parent_id = chosen.thread_id
                depth = chosen.depth + 1
        workers.append(
            _Worker(
                thread_id=f"w{index:05d}",
                parent_id=parent_id,
                depth=depth,
                spawn_tool_id=f"toolu-spawn-{index:05d}",
                description=f"{rng.choice(_ACTIONS)} {rng.choice(_TOPICS)}",
                agent_type=rng.choice(("researcher", "implementer", "reviewer", "operator")),
                start_ms=start_ms,
                end_ms=end_ms,
            )
        )
    return tuple(workers)


def _assistant_text(thread: _Thread, at_ms: int, text: str, *, final: bool) -> None:
    thread.add(
        at_ms,
        "answer" if final else "note",
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "end_turn" if final else "tool_use",
                "content": [{"type": "text", "text": text}],
            },
        },
    )


def _emit_tool_call(
    thread: _Thread,
    rng: random.Random,
    at_ms: int,
    duration_ms: int,
) -> int:
    """Write one tool_use and its matching tool_result; return the instant it completed."""

    name = rng.choice(_TOOL_NAMES)
    thread.tools += 1
    call_id = f"toolu-{thread.thread_id}-{thread.tools:06d}"
    thread.add(
        at_ms,
        "use",
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [
                    {
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": _tool_input(rng, name),
                    }
                ],
            },
        },
    )
    ended = at_ms + duration_ms
    # One call in fourteen fails. A corpus in which everything succeeds never produces a failed
    # tool status, so nothing that renders one is ever drawn from generated material.
    failed = rng.randrange(14) == 0
    thread.add(
        ended,
        "result",
        {
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": (
                            f"{name} exited non-zero. {rng.choice(_OBSERVATIONS)}"
                            if failed
                            else _tool_output(rng, name)
                        ),
                        "is_error": failed,
                    }
                ],
            },
        },
    )
    return ended


def _emit_spawn(parent: _Thread, worker: _Worker) -> None:
    """Write the parent side of a fork: the Agent tool call the child's metadata points at."""

    parent.add(
        worker.start_ms,
        "spawn",
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [
                    {
                        "type": "tool_use",
                        "id": worker.spawn_tool_id,
                        "name": "Agent",
                        "input": {
                            "description": worker.description,
                            "subagent_type": worker.agent_type,
                            "prompt": (
                                f"Please {worker.description} and return the evidence you "
                                "relied on."
                            ),
                        },
                    }
                ],
            },
        },
    )
    parent.add(
        worker.start_ms + 1000,
        "spawned",
        {
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": worker.spawn_tool_id,
                        "content": "Agent launched.",
                        "is_error": False,
                    }
                ],
            },
        },
    )


def _emit_check_in(parent: _Thread, worker: _Worker, at_ms: int, message: str) -> None:
    """Write a parent's mid-flight message to a running child, and its delivery receipt."""

    call_id = f"toolu-msg-{worker.thread_id}"
    parent.add(
        at_ms,
        "send",
        {
            "type": "assistant",
            "message": {
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [
                    {
                        "type": "tool_use",
                        "id": call_id,
                        "name": "SendMessage",
                        "input": {"recipient": worker.thread_id, "message": message},
                    }
                ],
            },
        },
    )
    parent.add(
        at_ms + 1000,
        "sent",
        {
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": "Delivered.",
                        "is_error": False,
                    }
                ],
            },
        },
    )


def _write_thread(path: Path, thread: _Thread, session_id: str) -> int:
    """Serialize one transcript in time order, chaining each record to the one before it."""

    lines: list[str] = []
    previous: JsonValue = None
    for _, _, record in sorted(thread.records, key=lambda item: (item[0], item[1])):
        record["sessionId"] = session_id
        record["isSidechain"] = thread.thread_id != session_id
        record["parentUuid"] = previous
        if thread.thread_id != session_id:
            record["agentId"] = thread.thread_id
        previous = record["uuid"]
        lines.append(json.dumps(record, ensure_ascii=False, separators=(",", ":"), sort_keys=True))
    path.parent.mkdir(parents=True, exist_ok=True)
    write_text_if_changed(path, "\n".join(lines) + "\n")
    return len(lines)


def _emit_coordinator_days(
    coordinator: _Thread, scale: SyntheticScale, rng: random.Random
) -> None:
    """Give the coordinator a working day of its own on every day of the corpus.

    Without this the coordinator track would exist only at the instants it forks something, and
    the top-level lifetime -- the one every other track hangs under -- would be a row of dots.
    """

    for day in range(scale.days):
        day_start = scale.start_ms + day * _DAY_MS
        prompt_at = day_start + rng.randrange(0, 30) * _MINUTE_MS
        coordinator.add(
            prompt_at,
            "prompt",
            {
                "type": "user",
                "origin": {"kind": "human"},
                "message": {
                    "role": "user",
                    "content": (
                        f"Day {day + 1}: {rng.choice(_ACTIONS)} {rng.choice(_TOPICS)} "
                        "and report what the evidence shows."
                    ),
                },
            },
        )
        at_ms = prompt_at + 2 * _MINUTE_MS
        for _ in range(3):
            _assistant_text(coordinator, at_ms, _sentence(rng), final=False)
            at_ms = _emit_tool_call(
                coordinator, rng, at_ms + _MINUTE_MS, rng.randrange(10, 240) * 1000
            )
            at_ms = min(
                at_ms + rng.randrange(5, 90) * _MINUTE_MS, day_start + _WORK_WINDOW_MS
            )
        _assistant_text(
            coordinator,
            day_start + _WORK_WINDOW_MS,
            f"Day {day + 1} closed. {rng.choice(_OBSERVATIONS)}",
            final=True,
        )


def _emit_worker(
    child: _Thread, parent: _Thread, worker: _Worker, scale: SyntheticScale, rng: random.Random
) -> None:
    """Write one subagent's whole life: its instruction, its work, and its answer."""

    child.add(
        worker.start_ms,
        "instruction",
        {
            "type": "user",
            "message": {
                "role": "user",
                "content": f"Please {worker.description} and return the evidence you relied on.",
            },
        },
    )
    calls = max(2, int(rng.gauss(scale.tool_calls_per_agent, scale.tool_calls_per_agent / 4)))
    # PACE THE WORK TO THE LIFETIME, rather than sprinkling calls at a fixed rate until the agent
    # dies. A fixed rate makes `tool_calls_per_agent` a wish: a short-lived agent stops long before
    # reaching it, and the corpus lands at a fraction of the requested size for reasons nothing
    # reports. Deriving the spacing from the lifetime means the requested count is what is written
    # and the density matches a captured archive's -- roughly a hundred calls in a couple of hours.
    first_ms = worker.start_ms + _MINUTE_MS
    last_ms = worker.end_ms - 30_000
    step_ms = max(5_000, (last_ms - first_ms) // calls)
    at_ms = first_ms
    # Bursts, not a uniform sprinkle: several calls close together, then a longer pause with a note
    # in between. That is what puts alternating tool and idle strips inside a phase rather than one
    # flat band, and the pause is where a reader sees an agent thinking.
    for call_index in range(calls):
        if at_ms >= last_ms:
            break
        at_ms = _emit_tool_call(
            child, rng, at_ms, rng.randrange(5_000, max(6_000, step_ms // 2))
        )
        if call_index % 4 == 3:
            _assistant_text(child, at_ms + 1000, _sentence(rng), final=False)
            at_ms += rng.randrange(step_ms, 2 * step_ms)
        else:
            at_ms += rng.randrange(step_ms // 4 + 1, step_ms // 2 + 2)
    if rng.random() < 0.35:
        _emit_check_in(
            parent,
            worker,
            worker.start_ms + (worker.end_ms - worker.start_ms) // 2,
            f"Also cover {rng.choice(_TOPICS)} before you finish.",
        )
    # One agent in twelve never answers. A corpus where every thread ends cleanly would leave the
    # unfinished-agent rendering -- a real and common state -- undrawn by any test.
    if rng.random() >= 0.08:
        _assistant_text(
            child,
            worker.end_ms - 1000,
            f"Finished: {worker.description}. {rng.choice(_OBSERVATIONS)}",
            final=True,
        )


def _generate_team(root: Path, scale: SyntheticScale, team_index: int) -> SyntheticTeam:
    rng = random.Random(f"wrkviz-synthetic:{scale.seed}:{team_index}")
    root_id = _thread_uuid(scale.seed, team_index)
    slug = f"synthetic-team-{team_index + 1}"
    team_root = root / slug
    session_file = team_root / f"{root_id}.jsonl"
    subagents = team_root / root_id / "subagents"

    workers = _plan_workers(scale, root_id, rng)
    coordinator = _Thread(root_id, [])
    threads: dict[str, _Thread] = {root_id: coordinator}
    for worker in workers:
        threads[worker.thread_id] = _Thread(worker.thread_id, [])

    _emit_coordinator_days(coordinator, scale, rng)
    for worker in workers:
        _emit_spawn(threads[worker.parent_id], worker)
        _emit_worker(threads[worker.thread_id], threads[worker.parent_id], worker, scale, rng)

    records = _write_thread(session_file, coordinator, root_id)
    for worker in workers:
        records += _write_thread(
            subagents / f"agent-{worker.thread_id}.jsonl", threads[worker.thread_id], root_id
        )
        meta: dict[str, JsonValue] = {
            "agentType": worker.agent_type,
            "description": worker.description,
            "parentAgentId": worker.parent_id,
            "spawnDepth": worker.depth,
            "toolUseId": worker.spawn_tool_id,
        }
        write_text_if_changed(
            subagents / f"agent-{worker.thread_id}.meta.json",
            json.dumps(meta, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n",
        )
        records += 1

    end_ms = max(
        [scale.start_ms + (scale.days - 1) * _DAY_MS + _WORK_WINDOW_MS]
        + [worker.end_ms for worker in workers]
    )
    return SyntheticTeam(
        slug=slug,
        root_thread_id=root_id,
        session_file=session_file,
        agents=len(workers) + 1,
        tool_calls=sum(thread.tools for thread in threads.values()),
        records=records,
        start_ms=scale.start_ms,
        end_ms=end_ms,
    )


def generate_sources(root: Path, scale: SyntheticScale) -> SyntheticCorpus:
    """Write every team's transcripts under ``root`` and return what was written.

    The same ``scale`` always writes the same bytes. Files already holding those bytes are left
    alone, so regenerating over an existing directory does not disturb their timestamps.
    """

    if scale.teams < 1 or scale.agents < 1 or scale.days < 1:
        raise ValueError("a synthetic corpus needs at least one team, agent and day")
    teams = tuple(
        _generate_team(root, scale, index) for index in range(scale.teams)
    )
    return SyntheticCorpus(scale=scale, root=root, teams=teams)


def build_corpus(
    corpus: SyntheticCorpus,
    archive: Path,
    *,
    display_timezone: str = "UTC",
    site: Path | None = None,
) -> dict[str, int]:
    """Ingest, summarize and build the generated corpus into a servable archive.

    Summaries come from the deterministic offline backend, so this spends no model tokens and
    produces the same archive every time. With more than one team the site is composed by the
    combining build, which is the path a multi-team archive really takes.
    """

    # Imported here rather than at module scope: writing transcripts needs nothing but the
    # standard library, and a caller that only wants the corpus should not pay for the whole
    # ingest and build stack to be imported.
    from wrkviz.multi_team import build_combined_archive
    from wrkviz.pipeline import build_archive, ingest_claude, summarize_archive

    for team in corpus.teams:
        ingest_claude(archive, team.session_file, team.slug, display_timezone)
        summarize_archive(archive, team.slug, "heuristic", "synthetic")
    slugs = tuple(team.slug for team in corpus.teams)
    if len(slugs) == 1:
        return build_archive(archive, slugs[0], output=site)
    return build_combined_archive(
        archive,
        slugs,
        output=site if site is not None else archive,
        display_timezone=display_timezone,
    )


def fingerprint(scale: SyntheticScale) -> str:
    """Return a digest binding a generated corpus to the code that would produce it.

    A cached corpus is only reusable while both halves are unchanged: the requested size, and the
    package that turns it into an archive. Hashing the package as well as the size is what stops a
    change to the builder from being validated against material an older builder produced.
    """

    digest = hashlib.sha256()
    digest.update(json.dumps(scale.to_json_obj(), sort_keys=True).encode("utf-8"))
    package = Path(__file__).resolve().parent
    for path in sorted(package.rglob("*.py")) + sorted((package / "static").rglob("*")):
        if not path.is_file() or "__pycache__" in path.parts:
            continue
        digest.update(path.relative_to(package).as_posix().encode("utf-8"))
        digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def _scale_from_args(namespace: argparse.Namespace) -> SyntheticScale:
    raw_preset: object = namespace.preset
    scale = PRESETS[str(raw_preset)]
    overrides: dict[str, int] = {}
    for field in ("teams", "agents", "days", "tool_calls_per_agent", "seed"):
        value: object = getattr(namespace, field)
        if value is not None:
            overrides[field] = int(str(value))
    return replace(scale, **overrides)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python3 -m wrkviz.synthetic",
        description="write deterministic synthetic transcripts, and optionally build them",
    )
    parser.add_argument("--out", required=True, help="directory to write into")
    parser.add_argument(
        "--preset", default="ci", choices=sorted(PRESETS), help="named starting size"
    )
    parser.add_argument("--teams", type=int, default=None)
    parser.add_argument("--agents", type=int, default=None, help="worker agents per team")
    parser.add_argument("--days", type=int, default=None)
    parser.add_argument(
        "--tool-calls-per-agent", type=int, default=None, dest="tool_calls_per_agent"
    )
    parser.add_argument("--seed", type=int, default=None)
    parser.add_argument(
        "--build",
        action="store_true",
        help="also ingest, summarize and build a servable archive",
    )
    parser.add_argument(
        "--reuse",
        action="store_true",
        help="skip the work when the output already matches this size and this package",
    )
    parser.add_argument("--timezone", default="UTC", help="display timezone for the build")
    return parser


#: What a run writes under its output directory. Named, because a run that no longer matches its
#: stamp has to remove exactly these and nothing else.
_GENERATED = ("sources", "archive", "archive.build", "archive.sources")


def _discard_previous(out: Path) -> None:
    """Remove what an earlier run of this module left, and only that.

    A second run at a different size or seed cannot simply write over the first. The transcripts
    are ingested under an append-only rule -- a source that shrinks or is rewritten is a hard
    error, which is the correct behaviour for a captured log and the wrong one for a regenerated
    fixture -- and an archive still holding a team the new size does not produce would serve it.
    Only paths this module is known to have created are removed, evidenced by the stamp beside
    them, so pointing the tool at a directory holding anything else cannot delete it.
    """

    for name in _GENERATED:
        target = out / name
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)


def run(argv: Sequence[str]) -> int:
    """Generate a corpus from command-line arguments and print a JSON report."""

    namespace = _parser().parse_args(list(argv))
    scale = _scale_from_args(namespace)
    out = Path(str(namespace.out)).expanduser()
    stamp = out / "fingerprint.json"
    expected = fingerprint(scale)
    report_path = out / "report.json"
    recorded: JsonValue = None
    if stamp.is_file():
        recorded = json.loads(stamp.read_text(encoding="utf-8"))
    current = (
        isinstance(recorded, dict)
        and recorded.get("fingerprint") == expected
        and recorded.get("built") == bool(namespace.build)
    )
    if current and bool(namespace.reuse) and report_path.is_file():
        sys.stdout.write(report_path.read_text(encoding="utf-8"))
        return 0
    if stamp.is_file():
        stamp.unlink()
    if recorded is not None and not current:
        _discard_previous(out)
    corpus = generate_sources(out / "sources", scale)
    report = corpus.to_json_obj()
    if bool(namespace.build):
        counts = build_corpus(
            corpus,
            out / "archive",
            display_timezone=str(namespace.timezone),
        )
        report["archive"] = str(out / "archive")
        report["build"] = {key: value for key, value in sorted(counts.items())}
    text = json.dumps(report, indent=2, sort_keys=True) + "\n"
    out.mkdir(parents=True, exist_ok=True)
    report_path.write_text(text, encoding="utf-8")
    stamp.write_text(
        json.dumps({"fingerprint": expected, "built": bool(namespace.build)}, sort_keys=True)
        + "\n",
        encoding="utf-8",
    )
    sys.stdout.write(text)
    return 0


if __name__ == "__main__":  # pragma: no cover - module entry point
    raise SystemExit(run(sys.argv[1:]))


__all__ = [
    "DEFAULT_START_MS",
    "PRESETS",
    "SyntheticCorpus",
    "SyntheticScale",
    "SyntheticTeam",
    "build_corpus",
    "fingerprint",
    "generate_sources",
    "run",
]

#!/usr/bin/env python3
"""Morning report and flip candidates from the nightly pressure-test sweeps.

pressure_schedule.py appends one line per night to
ignored/ci-hub/pressure-sweeps.jsonl. A sweep that ran left a pressure-test
result directory whose summary.json gives every red cell's repeated verdict
(CLEAN, FLAKY, FAILING or INCOMPLETE over SWEEP_REPETITIONS first-attempt
runs) and whose runner-profile.json gives the steps' wall and CPU.

  summary     the latest night's cells per backend by verdict, and its cost
  candidates  cells CLEAN on the latest night and on the night before it
  morning     once per finished night, the summary for the coordinator: the
              health tick runs it every 30 minutes and it exits 1 (a note)
              only the first time it sees a night whose campaign has ended,
              so the report does not wait for anyone to be awake

A night has several parts (pressure_schedule.SWEEP_PARTS): the red sweep and
one survey-<backend> part per backend probe its cells three times; the parity
sample and the parity-all part (every parity-population cell once, disabled
candidates as probes on ptrace's inputs) give no verdicts and only add to the
night's cost. A candidate
from the red part is a cell to select; one from a survey part is a
not-applicable cell whose reason no longer holds. On Sunday's night the weekly
resurvey-<backend> parts probe every disabled cell once; they flip nothing, are
kept out of the nightly verdicts, and are reported in their own table with the
cells they promoted into the nightly survey population.

A flip candidate is evidence for a reviewed manifest change, not the change:
this script never edits a manifest or the scorecard.

Some red cells are red by design: a compat row's replay cell, its SaBRe cell
and the heavy rows' ptrace cells belong to their own run types (rr, SaBRe,
super), so the full validation never selects them and their scorecard rows
carry no not_selected_by_full_reason. Selecting one is a change to that run
type, not a manifest flip, so both commands list these apart as run-type
scoped (on 2026-10-20 they were 281 of 296 two-night CLEAN cells). A cell
its run type itself leaves unselected has a reason and still lists as a
candidate (compat/timeout@ptrace, compat/mpstat-softirqs@sabre): the scorecard
does not record the run type.

A cell on an archived backend is no flip candidate either (owner, 2026-10-25:
SaBRe and e9patch are archived to nightly-only tests). Hermit's roster,
ci/manifest-plan/src/archived.rs at the night's commit, names the archived
backends; their cells that full validation would select belong to the
archived-nightly run type, and the nightly sweep runs every one of their red
cells. A CLEAN streak there would select a cell back into the per-commit
validation of a backend that gets no feature work, so both commands list these
apart as archived, whether or not their row has a reason.

The archived lanes (sabre-examples, e9patch-corpus) are focused validate.rs
runs rather than cells; the summary reports their verdict, wall and CPU apart.
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import os
import re
import subprocess
import sys
import zoneinfo
from pathlib import Path
from typing import Iterable, Mapping, Sequence

SOURCE_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "series"))

import series  # noqa: E402

STATE_ROOT = Path(
    os.environ.get("DEV_HERMIT_PARENT", series.shared_parent_root(SOURCE_ROOT))
).resolve()
VERDICTS = ("CLEAN", "FLAKY", "FAILING", "INCOMPLETE")
BACKENDS = ("ptrace", "sabre", "native", "kvm", "dbt", "liteinst", "in-guest-trap")


class ReportError(RuntimeError):
    pass


def sweeps_path(root: Path) -> Path:
    return root / "ignored" / "ci-hub" / "pressure-sweeps.jsonl"


def nights(root: Path) -> dict[str, dict[str, dict]]:
    """Each night's latest record per part that ran, by night (YYYY-MM-DD)."""

    path = sweeps_path(root)
    if not path.is_file():
        return {}
    latest: dict[str, dict[str, dict]] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise ReportError(f"{path}:{number}: invalid JSON: {error}") from error
        if record.get("result") in ("ran", "sample"):
            latest.setdefault(record["night"], {})[record.get("part", "red")] = record
    return latest


def parts_not_run(root: Path, night: str) -> dict[str, dict]:
    """The night's latest record per part that did not run, by part.

    A part refused at admission, refused for width, skipped as too late, left
    unjudged or with nothing to run still has a line; the report names it
    rather than dropping it.
    """

    path = sweeps_path(root)
    found: dict[str, dict] = {}
    ran: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        if record.get("night") != night:
            continue
        part = record.get("part", "red")
        if record.get("result") in ("ran", "sample", LANE_RESULT):
            ran.add(part)
            found.pop(part, None)
        elif part not in ran:
            found[part] = record
    return found


#: The sweep line of an archived lane (pressure_schedule.run_archived_lane).
LANE_RESULT = "lane"


def lane_parts(root: Path, night: str) -> dict[str, dict]:
    """The night's latest archived-lane record per part."""

    path = sweeps_path(root)
    found: dict[str, dict] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        if record.get("night") == night and record.get("result") == LANE_RESULT:
            found[record.get("part", "")] = record
    return found


def cell_id(cell: Mapping[str, str]) -> str:
    return (
        f"{cell['lane']}/{cell['category']}/{cell['test']}/{cell['mode']}@{cell['backend']}"
    )


#: Parts whose repeated cells are not nightly verdicts: the parity sample, and
#: parity-all, which runs every population cell once for its parity post-pass
#: (a disabled candidate as a probe on ptrace's inputs, which is no evidence for
#: a flip) and overlaps the red part's cells.
NO_VERDICT_PARTS = ("sample", "parity-all")


def load_part(record: Mapping[str, object]) -> dict:
    """One part's summary, cost and per-cell verdicts, from its result directory."""

    results = Path(str(record["results"]))
    summary_path = results / "summary.json"
    if not summary_path.is_file():
        raise ReportError(f"{record['night']}: {summary_path} is missing")
    summary = json.loads(summary_path.read_text(encoding="utf-8"))
    cells = summary.get("repeated_cells")
    if record.get("part") in NO_VERDICT_PARTS:
        cells = []
    elif not isinstance(cells, list):
        raise ReportError(f"{summary_path} has no repeated_cells")
    verdicts = {}
    for cell in cells:
        verdict = cell.get("verdict")
        if verdict not in VERDICTS:
            raise ReportError(
                f"{summary_path}: {cell_id(cell['cell'])} has no repeated verdict "
                f"({verdict!r}); this summary predates per-cell verdicts"
            )
        verdicts[cell_id(cell["cell"])] = {
            "backend": cell["cell"]["backend"],
            "population": "red" if record.get("part", "red") == "red" else "disabled",
            "verdict": verdict,
            "clean_passes": cell.get("clean_passes"),
            "total": cell.get("total"),
        }
    wall = cpu = None
    profile_path = results / "runner-profile.json"
    if profile_path.is_file():
        profile = json.loads(profile_path.read_text(encoding="utf-8"))
        wall = float(profile.get("scheduler_wall_s") or 0.0)
        cpu = 0.0
        for row in profile.get("step_profile_rows", []):
            for key in ("user_s", "sys_s"):
                value = row.get(key)
                if value not in (None, ""):
                    cpu += float(value)
    return {
        "night": record["night"],
        "part": record.get("part", "red"),
        "results": str(results),
        "run": results.name,
        "hermit_sha": summary.get("hermit_sha"),
        "guests": record.get("guests"),
        "idle_cores": record.get("idle_cores"),
        "started_at": record.get("started_at"),
        "finished_at": record.get("finished_at"),
        "run_exit": record.get("run_exit"),
        "target_validation": record.get("target_validation"),
        "unavailable_cells": unavailable_count(summary.get("unavailable_cells")),
        "scheduler_wall_s": wall,
        "cpu_s": cpu,
        "verdicts": verdicts,
    }


def unavailable_count(value: object) -> int:
    """Hermit's pressure-test summary writes a count; older summaries wrote a list."""
    if isinstance(value, bool):
        raise TypeError(f"unavailable_cells is a boolean: {value!r}")
    if isinstance(value, int):
        return value
    return len(value or [])


def load_night(parts: Mapping[str, Mapping[str, object]]) -> dict:
    """A night: its red part's identity, every part's verdicts, the summed cost.

    The weekly resurvey parts add to the cost but not to the verdicts.
    """

    loaded = [load_part(record) for _, record in sorted(parts.items())]
    resurvey = {}
    for part in loaded:
        if part["part"].startswith("resurvey-"):
            counts = collections.Counter(v["verdict"] for v in part["verdicts"].values())
            resurvey[part["part"][len("resurvey-"):]] = {
                "counts": counts,
                "promoted": parts[part["part"]].get("promoted", 0),
            }
            part["verdicts"] = {}
    first = next((part for part in loaded if part["part"] == "red"), loaded[0])
    verdicts: dict[str, dict] = {}
    for part in loaded:
        for identity, verdict in part["verdicts"].items():
            if identity in verdicts:
                raise ReportError(f"{part['night']}: {identity} was swept by two parts")
            verdicts[identity] = {**verdict, "run": part["run"]}

    def total(key):
        values = [part[key] for part in loaded if part[key] is not None]
        return sum(values) if values else None

    starts = [part["started_at"] for part in loaded if part["started_at"]]
    ends = [part["finished_at"] for part in loaded if part["finished_at"]]
    return {
        **first,
        "parts": [part["part"] for part in loaded],
        "started_at": min(starts) if starts else None,
        "finished_at": max(ends) if ends else None,
        "unavailable_cells": total("unavailable_cells") or 0,
        "scheduler_wall_s": total("scheduler_wall_s"),
        "cpu_s": total("cpu_s"),
        "verdicts": verdicts,
        "resurvey": resurvey,
        "excluded": parts.get("red", {}).get("excluded"),
    }


def elapsed_seconds(night: Mapping[str, object]) -> float | None:
    try:
        start = dt.datetime.strptime(str(night["started_at"]), "%Y-%m-%dT%H:%M:%SZ")
        end = dt.datetime.strptime(str(night["finished_at"]), "%Y-%m-%dT%H:%M:%SZ")
    except (KeyError, ValueError):
        return None
    return (end - start).total_seconds()


def tally(night: Mapping[str, object]) -> dict[str, collections.Counter]:
    by_backend: dict[str, collections.Counter] = collections.defaultdict(collections.Counter)
    for cell in night["verdicts"].values():
        by_backend[cell["backend"]][cell["verdict"]] += 1
    return by_backend


# Where scorecard_cells looks for the Hermit commit, in order: main() puts
# the state root's hermit checkout first. The health tick runs this script
# from a copy of the tool whose own hermit/ is no usable checkout, so the
# morning reports it posted from 2026-10-21 to 2026-10-26 split no run-type
# scoped cell from the flip candidates (78 listed on 2026-10-26, 69 of them
# scoped).
HERMIT_CHECKOUTS: list[Path] = [SOURCE_ROOT / "hermit"]


def hermit_show(hermit_sha: str, path: str) -> tuple[bool, str | None]:
    """(whether one of HERMIT_CHECKOUTS has HERMIT_SHA, PATH's text at it in
    the first that does, or None when that commit has no such file)."""

    for checkout in HERMIT_CHECKOUTS:
        git = ["git", "-C", str(checkout)]
        try:
            present = subprocess.run(
                [*git, "cat-file", "-e", f"{hermit_sha}^{{commit}}"],
                capture_output=True,
                check=False,
            )
            if present.returncode != 0:
                continue
            shown = subprocess.run(
                [*git, "show", f"{hermit_sha}:{path}"],
                capture_output=True,
                text=True,
                check=False,
            )
        except OSError:
            continue
        return True, shown.stdout if shown.returncode == 0 else None
    return False, None


def scorecard_cells(hermit_sha: str) -> list[dict] | None:
    """ci/compat-envelope/cells.json at HERMIT_SHA from the first of
    HERMIT_CHECKOUTS that has it, or None when none can read it."""

    _, text = hermit_show(hermit_sha, "ci/compat-envelope/cells.json")
    if text is None:
        return None
    try:
        cells = json.loads(text).get("cells")
    except (ValueError, AttributeError):
        return None
    return cells if isinstance(cells, list) else None


def run_type_scoped(hermit_sha: object) -> frozenset[str] | None:
    """Red cells the full validation leaves to another run type: no
    not_selected_by_full_reason on their scorecard row. None when the
    scorecard at HERMIT_SHA cannot be read, so nothing is split."""

    cells = scorecard_cells(str(hermit_sha)) if hermit_sha else None
    if cells is None:
        return None
    return frozenset(
        cell_id(cell)
        for cell in cells
        if cell.get("status") == "red" and "not_selected_by_full_reason" not in cell
    )


#: Hermit's roster of archived execution paths, and one entry of it:
#: `Archived { name: "sabre", kind: ArchivedKind::Backend, ...`.
ARCHIVED_ROSTER = "ci/manifest-plan/src/archived.rs"
ARCHIVED_ENTRY_RE = re.compile(r'\bArchived\s*\{\s*name:\s*"([^"]+)",\s*kind:\s*ArchivedKind::(\w+)')


def archived_backends(hermit_sha: object) -> frozenset[str] | None:
    """The backends Hermit's roster archives at HERMIT_SHA (preprocessing
    entries such as e9patch have no cells and are left out); empty for a
    commit before the roster, None when no checkout has the commit."""

    if not hermit_sha:
        return None
    found, source = hermit_show(str(hermit_sha), ARCHIVED_ROSTER)
    if not found:
        return None
    if source is None:
        return frozenset()
    return frozenset(
        name for name, kind in ARCHIVED_ENTRY_RE.findall(source) if kind == "Backend"
    )


def is_flip(cell: Mapping[str, object]) -> bool:
    return not cell["scoped"] and not cell["archived"]


def candidates(latest: Mapping[str, object], previous: Mapping[str, object]) -> list[dict]:
    """Cells CLEAN on both nights, with each night's evidence. `scoped` marks a
    run-type scoped cell and `archived` a cell on an archived backend (module
    docstring), both judged at the latest night's hermit; each is False for
    every cell when what it reads there cannot be read. An archived cell is
    never also counted as scoped."""

    archived = archived_backends(latest.get("hermit_sha")) or frozenset()
    scoped = run_type_scoped(latest.get("hermit_sha")) or frozenset()
    found = []
    for identity, cell in sorted(latest["verdicts"].items()):
        before = previous["verdicts"].get(identity)
        if cell["verdict"] == "CLEAN" and before and before["verdict"] == "CLEAN":
            found.append(
                {
                    "cell": identity,
                    "backend": cell["backend"],
                    "population": cell["population"],
                    "scoped": identity in scoped and cell["backend"] not in archived,
                    "archived": cell["backend"] in archived,
                    "evidence": [
                        {
                            "night": night["night"],
                            "run": verdict["run"],
                            "hermit_sha": night["hermit_sha"],
                            "clean_passes": verdict["clean_passes"],
                            "total": verdict["total"],
                        }
                        for night, verdict in ((previous, before), (latest, cell))
                    ],
                }
            )
    return found


def last_two(root: Path) -> tuple[dict | None, dict | None]:
    """The latest night and, if it is the calendar night before, the one before it."""

    by_night = nights(root)
    if not by_night:
        return None, None
    ordered = sorted(by_night)
    latest = load_night(by_night[ordered[-1]])
    latest["not_run"] = parts_not_run(root, ordered[-1])
    latest["lanes"] = lane_parts(root, ordered[-1])
    previous = None
    if len(ordered) > 1:
        day = dt.date.fromisoformat(ordered[-1])
        if dt.date.fromisoformat(ordered[-2]) == day - dt.timedelta(days=1):
            previous = load_night(by_night[ordered[-2]])
    return latest, previous


def eastern(timestamp: object) -> str:
    """An ISO UTC timestamp as US Eastern wall time; the text as given when it
    does not parse."""

    try:
        moment = dt.datetime.fromisoformat(str(timestamp))
    except ValueError:
        return str(timestamp)
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=dt.timezone.utc)
    local = moment.astimezone(zoneinfo.ZoneInfo("America/New_York"))
    return f"{local:%Y-%m-%d} {local.hour % 12 or 12}:{local:%M %p} ET"


def target_line(validation: object) -> str:
    """Which commit the night swept and how it relates to Hermit main: the
    newest commit a regular full validation passed, and how far behind main
    (pressure_schedule.choose_target)."""

    if not isinstance(validation, Mapping):
        return "Target: not recorded (a night launched before the target was chosen by validation)"
    target = str(validation.get("target") or "")[:12]
    main = str(validation.get("main") or "")[:12]
    behind = validation.get("commits_behind_main")
    distance = f"{behind} commit(s) behind Hermit main {main}" if behind is not None else f"not in Hermit main {main}'s recent history"
    if validation.get("validated"):
        return (
            f"Target: {target}, validated by {validation.get('validated_by')} at "
            f"{eastern(validation.get('validated_at'))}, {distance}"
        )
    return f"Target: {target}, NOT validated by any regular full validation, {distance}"


def render_summary(
    latest: Mapping[str, object], previous: Mapping[str, object] | None
) -> str:
    counts = tally(latest)
    found = candidates(latest, previous) if previous else []
    flips = collections.Counter(cell["backend"] for cell in found if is_flip(cell))
    scoped = collections.Counter(cell["backend"] for cell in found if cell["scoped"])
    archived = collections.Counter(cell["backend"] for cell in found if cell["archived"])
    lines = [
        f"Nightly sweep {latest['night']} at hermit {str(latest['hermit_sha'])[:12]}, "
        f"{latest['guests']} guests ({latest['idle_cores']} cores idle at admission), "
        f"parts {', '.join(latest['parts'])}",
        target_line(latest.get("target_validation")),
        "",
        "| backend | cells | CLEAN | FLAKY | FAILING | INCOMPLETE | flip candidates |",
        "|---|---:|---:|---:|---:|---:|---:|",
    ]
    totals: collections.Counter = collections.Counter()
    for backend in [*BACKENDS, *sorted(set(counts) - set(BACKENDS))]:
        row = counts.get(backend, collections.Counter())
        cells = sum(row.values())
        if not cells:
            continue
        totals.update(row)
        totals["cells"] += cells
        totals["flips"] += flips[backend]
        lines.append(
            f"| {backend} | {cells} | "
            + " | ".join(str(row[verdict]) for verdict in VERDICTS)
            + f" | {flips[backend]} |"
        )
    lines.append(
        f"| all | {totals['cells']} | "
        + " | ".join(str(totals[verdict]) for verdict in VERDICTS)
        + f" | {totals['flips']} |"
    )
    elapsed = elapsed_seconds(latest)
    cost = []
    if elapsed is not None:
        cost.append(f"admitted wall {elapsed:.0f} s")
    if latest["scheduler_wall_s"] is not None:
        cost.append(f"scheduler wall {latest['scheduler_wall_s']:.0f} s")
    if latest["cpu_s"] is not None:
        cost.append(f"step CPU {latest['cpu_s']:.0f} CPU-s")
    if latest.get("resurvey"):
        lines += [
            "",
            "Weekly re-survey (one run each; flips nothing):",
            "",
            "| backend | cells | CLEAN | FLAKY | FAILING | INCOMPLETE | promoted |",
            "|---|---:|---:|---:|---:|---:|---:|",
        ]
        for backend in [*BACKENDS, *sorted(set(latest["resurvey"]) - set(BACKENDS))]:
            if backend not in latest["resurvey"]:
                continue
            row = latest["resurvey"][backend]
            lines.append(
                f"| {backend} | {sum(row['counts'].values())} | "
                + " | ".join(str(row["counts"][verdict]) for verdict in VERDICTS)
                + f" | {row['promoted']} |"
            )
    excluded = latest.get("excluded")
    if excluded and excluded.get("count"):
        lines += [
            "",
            f"Excluded from the red part by name: {excluded['count']} cell(s), "
            f"reason: {excluded['reason']}",
        ]
    lanes = latest.get("lanes") or {}
    if lanes:
        lines += ["", "Archived lanes (focused validate.rs runs, not cells):"]
        for part in sorted(lanes):
            lines.append(lane_line(part, lanes[part]))
    not_run = latest.get("not_run") or {}
    if not_run:
        lines += ["", "Parts that did not run:"]
        for part in sorted(not_run):
            record = not_run[part]
            detail = record.get("detail")
            code = record.get("admission_exit", record.get("run_exit"))
            lines.append(
                f"- {part}: {record.get('result')}"
                + (f" (exit {code})" if code is not None else "")
                + (f"; {detail}" if detail else "")
            )
    lines += [
        "",
        "Cost, all parts: " + (", ".join(cost) if cost else "not recorded"),
        f"Unavailable (no executable command): {latest['unavailable_cells']}",
        (
            f"Flip candidates need CLEAN on {previous['night']} and {latest['night']}."
            if previous
            else "No sweep on the night before, so no flip candidates yet."
        ),
    ]
    if scoped:
        lines.append(
            "Also CLEAN on both nights but run-type scoped, not counted as flip "
            f"candidates: {sum(scoped.values())} ("
            + ", ".join(f"{backend} {count}" for backend, count in sorted(scoped.items()))
            + ")"
        )
    if archived:
        lines.append(
            "Also CLEAN on both nights but on an archived backend (nightly only), "
            f"not counted as flip candidates: {sum(archived.values())} ("
            + ", ".join(f"{backend} {count}" for backend, count in sorted(archived.items()))
            + ")"
        )
    return "\n".join(lines) + "\n"


def lane_line(part: str, record: Mapping[str, object]) -> str:
    """One archived lane's verdict and cost, as its sweep line records them."""

    cost = [
        f"{label} {record[key]:.0f} s"
        for key, label in (("validate_wall_s", "wall"), ("wall_s", "part wall"))
        if isinstance(record.get(key), (int, float))
    ]
    if isinstance(record.get("validate_cpu_s"), (int, float)):
        cost.append(f"CPU {record['validate_cpu_s']:.0f} CPU-s")
    return (
        f"- {part} ({record.get('lane')}): {record.get('final_status') or 'no verdict'}, "
        f"exit {record.get('validate_exit')}"
        + (f", {', '.join(cost)}" if cost else "")
        + f"; log {record.get('log')}"
    )


#: The campaign's own state (pressure_schedule.state_path); a night is
#: reportable once its state is no longer launching or running.
CAMPAIGN_STATE = Path("ignored/ci-hub/pressure-test-state.json")
#: The last night the morning report was posted for, and where its reports go.
MORNING_MARKER = Path("ignored/ci-hub/sweep-morning-report.json")
MORNING_REPORTS = Path("ignored/ci-hub/sweep-reports")


def morning(root: Path) -> tuple[int, str]:
    """Post the latest finished night once: (1, its summary line) the first
    time, (0, why not) otherwise. Writes the full summary and candidate list
    to MORNING_REPORTS/<night>.md and records the night in MORNING_MARKER."""

    try:
        campaign = json.loads((root / CAMPAIGN_STATE).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        campaign = {}
    if campaign.get("state") in ("launching", "running"):
        return 0, f"nightly sweep still {campaign['state']}; no morning report yet"
    latest, previous = last_two(root)
    if latest is None:
        return 0, "no nightly sweep has run"
    try:
        posted = json.loads((root / MORNING_MARKER).read_text(encoding="utf-8")).get("night")
    except (OSError, ValueError):
        posted = None
    if posted == latest["night"]:
        return 0, f"morning report for {latest['night']} already posted"
    found = candidates(latest, previous) if previous else []
    flips = [cell["cell"] for cell in found if is_flip(cell)]
    scoped = sum(1 for cell in found if cell["scoped"])
    archived = sum(1 for cell in found if cell["archived"])
    body = render_summary(latest, previous)
    if flips:
        body += "\nFlip candidates (CLEAN on both nights):\n" + "".join(
            f"- {cell}\n" for cell in flips)
    else:
        body += "\nFlip candidates: none\n"
    report = root / MORNING_REPORTS / f"{latest['night']}.md"
    try:
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(body, encoding="utf-8")
    except OSError as error:
        raise ReportError(f"cannot write {report}: {error}") from error
    counts = tally(latest)
    backends = "; ".join(
        f"{backend} {sum(row.values())} ({', '.join(f'{v} {row[v]}' for v in VERDICTS if row[v])})"
        for backend in [*BACKENDS, *sorted(set(counts) - set(BACKENDS))]
        if (row := counts.get(backend)) and sum(row.values())
    )
    not_run = latest.get("not_run") or {}
    line = (
        f"night {latest['night']} at hermit {str(latest['hermit_sha'])[:12]}: {backends}. "
        f"Flip candidates {len(flips)}"
        + (f": {', '.join(flips)}" if flips else "")
        + (f"; {scoped} more are run-type scoped" if scoped else "")
        + (f"; {archived} more are on an archived backend" if archived else "")
        + (
            ". Archived lanes: "
            + ", ".join(
                f"{part} {record.get('final_status') or 'no verdict'}"
                for part, record in sorted((latest.get("lanes") or {}).items())
            )
            if latest.get("lanes")
            else ""
        )
        + (f". Parts that did not run: {', '.join(f'{part} {record.get("result")}' for part, record in sorted(not_run.items()))}" if not_run else "")
        + f". Full report: {report}"
    )
    try:
        (root / MORNING_MARKER).write_text(
            json.dumps({"night": latest["night"], "report": str(report)}) + "\n",
            encoding="utf-8")
    except OSError as error:
        line += f" (could not record the night in {MORNING_MARKER}: {error}; it will post again)"
    return 1, line


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--state-root", type=Path, default=STATE_ROOT)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("summary", help="the latest night per backend, and its cost")
    listing = subparsers.add_parser("candidates", help="cells CLEAN on two consecutive nights")
    listing.add_argument("--json", action="store_true", help="print JSON")
    subparsers.add_parser("morning", help="post the latest finished night once, as a note")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    HERMIT_CHECKOUTS[:] = list(dict.fromkeys([args.state_root / "hermit", SOURCE_ROOT / "hermit"]))
    if args.command == "morning":
        try:
            code, line = morning(args.state_root)
        except ReportError as error:
            print(f"summary=sweep_report: {error}")
            return 1
        print(f"summary={line}")
        return code
    try:
        latest, previous = last_two(args.state_root)
    except ReportError as error:
        print(f"sweep_report: {error}", file=sys.stderr)
        return 1
    if latest is None:
        print(f"sweep_report: no sweep has run ({sweeps_path(args.state_root)})", file=sys.stderr)
        return 1
    if args.command == "summary":
        sys.stdout.write(render_summary(latest, previous))
        return 0
    found = candidates(latest, previous) if previous else []
    if args.json:
        print(json.dumps(found, indent=1, sort_keys=True))
    else:
        for cell in found:
            if is_flip(cell):
                print(cell["cell"])
        scoped = [cell["cell"] for cell in found if cell["scoped"]]
        if scoped:
            print(f"# run-type scoped, not a config flip: {len(scoped)}")
            for identity in scoped:
                print(f"# {identity}")
        archived = [cell["cell"] for cell in found if cell["archived"]]
        if archived:
            print(f"# archived backend, nightly only, not a config flip: {len(archived)}")
            for identity in archived:
                print(f"# {identity}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

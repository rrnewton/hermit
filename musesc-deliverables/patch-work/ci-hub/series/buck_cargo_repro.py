#!/usr/bin/env python3
"""Nightly check that a Cargo-runner full validate reproduces the Buck one.

Since https://github.com/rrnewton/dev-hermit/commit/9ae69e93a3709f04d0043ab41218b057b1bdfd11
a plain `full` validate on this host runs its end-to-end (E2E) manifest cells
through Buck (`--e2e-runner buck-hybrid`) instead of Cargo. Owner request 084
keeps the Cargo runner from rotting: once a night, take the newest COMPLETED
Buck-runner full validate of rrnewton/hermit recorded on this host, run (or
reuse) a Cargo-runner full validate of the same exact commit, and compare.

Terms used below:

* node: one step of the validate graph, recorded in the ledger row's `gates`
  (for example `e2e.manifest_compat`). The Buck plan replaces each of the 16
  E2E bucket nodes and `full-scorecard.compatibility` with a twin whose name
  ends in `_buck`; a twin is compared under its Cargo name.
* cell: one (lane, category, test, mode, backend) of an E2E bucket. Its
  verdict is derived from every attempt row exactly as Hermit's
  `outcome_after_retries` (ci/manifest-plan/src/runner.rs) derives it. The
  rows are read from the run's retained `results.jsonl` artifacts, each
  checked byte-for-byte against the ledger row's raw-result census.

Outcomes:

* match: the overall verdicts agree, every node and every cell present in both
  runs has the same verdict, no cell is missing from a bucket that completed
  in the other run, and at least one cell was compared.
* mismatch: the overall verdict differs, a node or cell present in both runs
  has a differing verdict, or a cell appears in only one run although the
  other run's bucket completed (the two runs then executed different cell
  populations, which is coverage drift rather than a verdict).
* non-verdict: anything that prevented the comparison. A non-verdict is
  never reported as a match.
* buck-infra-broken: the newest eligible Buck run has cells that ended in an
  infrastructure ERROR (the harness refused or lost them, so they never ran;
  Hermit marks those rows `error_kind: infrastructure`). That run is not
  compared, because each such cell would read as a mismatch; the newest clean
  Buck run is compared instead, if there is one. The summary leads with
  "BUCK INFRA BROKEN". A mismatch or non-verdict of the clean run keeps its own
  outcome, and a match of it is reported as buck-infra-broken, never as a match.

`launch` is the daily health-tick gate: it never reads the ledger, repeats the
previous run's result, and starts `_run` as a detached user unit. `report` is
the every-tick gate: it reads the result the worker wrote, so a finished
result reaches tick-hub at the next tick rather than at the next launch, and
it stays raised until a result is a match.
`compare` compares two recorded runs by id without launching anything.
`status` prints the newest launch and its result without changing anything.
"""

from __future__ import annotations

import argparse
import collections
import dataclasses
import datetime as dt
import fcntl
import hashlib
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import time
import traceback
from pathlib import Path
from typing import Callable, Iterable, Mapping, Sequence


SOURCE_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "series"))
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "lib"))
import measurement_hold  # noqa: E402
import series  # noqa: E402


STATE_ROOT = Path(
    os.environ.get("DEV_HERMIT_PARENT", series.shared_parent_root(SOURCE_ROOT))
).resolve()
REPO = "rrnewton/hermit"
REPO_NAMES = frozenset({"hermit", REPO})
AGENT = "tick-buck-cargo-repro"
UNIT = "hermit-buck-cargo-repro"
BUCK_RUNNERS = ("buck-local", "buck-hybrid")
VERDICTS = ("pass", "fail")
SHA_RE = re.compile(r"[0-9a-f]{40}")
FLAKY_CELLS = SOURCE_ROOT / "ci-hub" / "validate" / "flaky-cells.json"

# validate-run's own bounds, unchanged: up to --wait seconds in the
# validate-lock queue, then the default 4800 s child deadline for Hermit.
VALIDATE_WAIT_SECONDS = 7200
VALIDATE_CHILD_DEADLINE_SECONDS = 4800
VALIDATE_PROCESS_TIMEOUT_SECONDS = (
    VALIDATE_WAIT_SECONDS + VALIDATE_CHILD_DEADLINE_SECONDS + 900
)
# A finished validate's row reaches the ledger spool shortly after the waiter
# returns; read the ledger a few times before calling it missing.
LEDGER_POLL_ATTEMPTS = 3
LEDGER_POLL_SECONDS = 60
FETCH_TIMEOUT_SECONDS = 120
UNIT_RUNTIME_SECONDS = (
    600  # ledger read, Hermit main fetch, ancestry checks
    + VALIDATE_PROCESS_TIMEOUT_SECONDS
    + LEDGER_POLL_ATTEMPTS * (LEDGER_POLL_SECONDS + 60)
)
# Newest-first Buck candidates examined for one that is on Hermit main.
MAX_CANDIDATES = 20
MAX_ATTEMPTS_PER_CELL = 2
CELL_OUTCOMES = ("PASS", "FAIL", "ERROR", "HOST-INAPPLICABLE")
CELL_FIELDS = ("lane", "category", "test", "mode", "backend")
BUCKET_PATH_RE = re.compile(r"(portable|privileged)/(manifest_[a-z0-9_]+)/results\.jsonl")
BUCKET_NODE_RE = re.compile(r"(?:e2e|privileged-e2e)\.manifest_[a-z0-9_]+")
# Node outcomes that mean "did not run to completion".
INCOMPLETE_GATE_KEYS = ("timed_out", "cpu_timed_out", "aborted", "oomed")
STATUS_RANK = {
    "planned-not-run": 0,
    "no-result": 1,
    "intentional-skip": 2,
    "dependency-skipped": 3,
    "pass": 4,
    "fail": 4,
}
# One-sided nodes the Buck full plan creates by design (hermit
# scripts/validate.rs, test a_buck_full_plan_swaps_exactly_the_buckets_for_their_buck_twins).
# They are labelled in the report; no node's absence decides the outcome,
# because cell coverage is checked bucket by bucket.
EXPECTED_BUCK_ONLY_NODES = frozenset({"e2e.buck_stage", "e2e.buck_cells"})
EXPECTED_CARGO_ONLY_NODES = frozenset(
    {
        "build.host_hermit_link",
        "build.manifest_guests_in_pinned_root",
        "compatprep.fixtures",
        "privileged-build.manifest_guests_in_pinned_root",
        "setup.manifest_plan_in_pinned_root",
    }
)
# validate-run's operational-tool route needs this, as for the
# integration-tip launcher (ci-hub/health/integration_tip_validate.py).
UNIT_GIT_SETENV: Mapping[str, str] = {"GIT_ALLOW_PROTOCOL": "https:file:ssh"}
CPU_PLACEMENT_OVERRIDE_ENV = "DEV_HERMIT_EXCLUDE_CPUS"
LIST_LIMIT = 5


class EvidenceError(Exception):
    """A run's recorded cell evidence is missing or does not verify."""


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def compact_stamp(moment: dt.datetime) -> str:
    return moment.astimezone(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def emit(state: str, summary: str) -> None:
    print(f"state={state}")
    print(f"summary={summary}")


def state_directory(root: Path) -> Path:
    return root / "ignored" / "ci-hub"


def log_path(root: Path) -> Path:
    return state_directory(root) / "buck-cargo-repro.log"


def launch_lock_path(root: Path) -> Path:
    return state_directory(root) / "buck-cargo-repro-launch.lock"


def launch_record_path(root: Path) -> Path:
    return state_directory(root) / "buck-cargo-repro-launch.json"


def state_path(root: Path) -> Path:
    return state_directory(root) / "buck-cargo-repro-state.json"


def reports_root(root: Path) -> Path:
    return state_directory(root) / "buck-cargo-repro"


def read_json(path: Path) -> dict | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def write_json(path: Path, value: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.tmp-{os.getpid()}")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def short(sha: object) -> str:
    return sha[:12] if isinstance(sha, str) and sha else "nosha"


def number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


# ---------------------------------------------------------------- selection


def runner_of(row: Mapping[str, object]) -> str:
    """The row's E2E runner; rows from before the field existed used Cargo."""
    return str(row.get("e2e_runner") or "cargo")


def completed_full(row: object, host: str | None) -> bool:
    """A finished, clean, full-profile Hermit validate with a verdict."""
    if not isinstance(row, dict):
        return False
    if row.get("repo") not in REPO_NAMES:
        return False
    if host is not None and row.get("host") != host:
        return False
    if row.get("profile") != "full" or row.get("selection_mode") != "full":
        return False
    if row.get("result") not in VERDICTS:
        return False
    exit_code = row.get("exit_code")
    if isinstance(exit_code, bool) or not isinstance(exit_code, int):
        return False
    if row.get("interruption_signal") is not None:
        return False
    commit = row.get("commit")
    if not isinstance(commit, str) or SHA_RE.fullmatch(commit) is None:
        return False
    if row.get("tree_dirty") is True:
        return False
    gates = row.get("gates")
    if not isinstance(gates, list) or not gates:
        return False
    run_id = row.get("run_id")
    return isinstance(run_id, str) and bool(run_id)


def newest_first(rows: Iterable[dict]) -> list[dict]:
    return sorted(
        rows,
        key=lambda row: (str(row.get("finished_at") or ""), str(row.get("run_id"))),
        reverse=True,
    )


def find_cargo_run(rows: Sequence[dict], host: str, commit: str) -> dict | None:
    """The newest completed Cargo-runner full validate of `commit` on `host`."""
    matches = [
        row
        for row in rows
        if completed_full(row, host)
        and runner_of(row) == "cargo"
        and row.get("commit") == commit
    ]
    return newest_first(matches)[0] if matches else None


def select_buck_run(
    rows: Sequence[dict],
    host: str,
    on_main: Callable[[str], bool | None],
    limit: int = MAX_CANDIDATES,
    infrastructure_errors: Callable[[dict], list[dict]] | None = None,
) -> tuple[dict | None, dict | None, list[dict]]:
    """Return (Buck row, existing Cargo row or None, skipped candidates).

    The newest completed Buck-runner full validate on this host is chosen,
    unless its commit is not on Hermit main and no Cargo run of it exists:
    validate-run --admit-main-ancestor refuses such a target, so the next
    older Buck run is tried instead and the skip is reported.

    A candidate whose cells ended in infrastructure ERRORs (as reported by
    `infrastructure_errors`) did not run those cells, so comparing it would
    count every one as a mismatch. It is skipped with
    `infrastructure_cells` set, and the next older candidate is tried.
    """

    candidates = newest_first(
        row
        for row in rows
        if completed_full(row, host) and runner_of(row) in BUCK_RUNNERS
    )
    skipped: list[dict] = []
    for row in candidates[:limit]:
        commit = str(row["commit"])
        cargo = find_cargo_run(rows, host, commit)
        ancestry = True if cargo is not None else on_main(commit)
        if ancestry is True:
            broken = infrastructure_errors(row) if infrastructure_errors is not None else []
            if not broken:
                return row, cargo, skipped
            skipped.append(
                {
                    "run_id": row["run_id"],
                    "commit": commit,
                    "reason": f"{len(broken)} cell(s) ended in a Buck infrastructure ERROR",
                    "infrastructure_cells": len(broken),
                    "examples": broken[:LIST_LIMIT],
                }
            )
            continue
        skipped.append(
            {
                "run_id": row["run_id"],
                "commit": commit,
                "reason": (
                    "not on Hermit origin/main"
                    if ancestry is False
                    else "ancestry against Hermit origin/main unknown"
                ),
            }
        )
    return None, None, skipped


# ---------------------------------------------------------------- evidence


def cell_verdict(history: Sequence[tuple[object, object]]) -> tuple[str | None, str | None]:
    """Mirror hermit runner.rs `outcome_after_retries`: (verdict, error)."""

    expected = 1
    saw = set()
    for attempt, outcome in history:
        if isinstance(attempt, bool) or not isinstance(attempt, int):
            return None, f"attempt {attempt!r} is not an integer"
        if attempt > MAX_ATTEMPTS_PER_CELL:
            return None, (
                f"attempt {attempt} exceeds the shared maximum of {MAX_ATTEMPTS_PER_CELL}"
            )
        if attempt != expected:
            return None, (
                f"attempt {attempt} does not follow the preceding attempts; expected {expected}"
            )
        if "PASS" in saw or "HOST-INAPPLICABLE" in saw:
            terminal = "PASS" if "PASS" in saw else "HOST-INAPPLICABLE"
            return None, f"attempt {attempt} follows terminal outcome {terminal}"
        if outcome not in CELL_OUTCOMES:
            return None, f"attempt {attempt} has unknown outcome {outcome!r}"
        saw.add(outcome)
        expected += 1
    if expected == 1:
        return None, "the cell has no attempts"
    if "HOST-INAPPLICABLE" in saw and saw & {"FAIL", "ERROR"}:
        return None, "HOST-INAPPLICABLE cannot be mixed with executed attempts"
    for verdict in ("PASS", "FAIL", "ERROR", "HOST-INAPPLICABLE"):
        if verdict in saw:
            return verdict, None
    raise AssertionError("unreachable")


def bucket_node(lane: str, bucket: str) -> str:
    return ("e2e." if lane == "portable" else "privileged-e2e.") + bucket


def cell_label(identity: Sequence[object]) -> str:
    lane, _category, test, mode, backend = identity
    return f"{lane}:{test}/{mode}/{backend if backend is not None else 'none'}"


def flaky_key(identity: Sequence[object]) -> str:
    _lane, _category, test, mode, backend = identity
    parts = [str(test), str(mode)] + ([str(backend)] if backend is not None else [])
    return "/".join(parts)


def sort_key(identity: Sequence[object]) -> tuple[str, ...]:
    return tuple("" if part is None else str(part) for part in identity)


def load_run_evidence(root: Path, row: Mapping[str, object]) -> dict:
    """Derive every cell verdict of one run from its census-verified artifacts."""

    run_id = row.get("run_id")
    commit = row.get("commit")
    census = row.get("raw_result_input_census_v1")
    if not isinstance(census, dict):
        raise EvidenceError(f"{run_id}: the ledger row has no raw-result census")
    if census.get("schema") != 1:
        raise EvidenceError(f"{run_id}: unsupported census schema {census.get('schema')!r}")
    if census.get("run_id") != run_id or census.get("hermit_sha") != commit:
        raise EvidenceError(f"{run_id}: the census is bound to another run or commit")
    files = census.get("files")
    if not isinstance(files, list):
        raise EvidenceError(f"{run_id}: the census lists no result files")
    base = root / "ignored" / "validate" / "artifacts" / str(run_id) / "e2e"
    histories: dict[tuple, list[tuple[int, int, object, dict]]] = collections.defaultdict(list)
    cell_nodes: dict[tuple, str] = {}
    buckets: dict[str, dict] = {}
    for entry in files:
        path = entry.get("path") if isinstance(entry, dict) else None
        match = BUCKET_PATH_RE.fullmatch(path) if isinstance(path, str) else None
        if match is None:
            raise EvidenceError(f"{run_id}: unexpected census path {path!r}")
        lane, bucket = match.groups()
        node = bucket_node(lane, bucket)
        if node in buckets:
            raise EvidenceError(f"{run_id}: the census lists {path} twice")
        artifact = base / path
        try:
            data = artifact.read_bytes()
        except OSError as error:
            raise EvidenceError(
                f"{run_id}: cannot read {artifact}: {error.strerror or error}"
            ) from error
        if len(data) != entry.get("bytes") or hashlib.sha256(data).hexdigest() != entry.get("sha256"):
            raise EvidenceError(
                f"{run_id}: {path} differs from the ledger census (bytes or sha256)"
            )
        try:
            lines = data.decode("utf-8").splitlines()
        except UnicodeDecodeError as error:
            raise EvidenceError(f"{run_id}: {path} is not UTF-8") from error
        census_rows = entry.get("rows")
        if not isinstance(census_rows, list):
            raise EvidenceError(f"{run_id}: the census has no rows for {path}")
        present = sum(1 for line in lines if line.strip())
        if present != len(census_rows):
            raise EvidenceError(
                f"{run_id}: {path} has {present} result rows but the census lists {len(census_rows)}"
            )
        seen: set[int] = set()
        category_suffix = bucket[len("manifest_"):]
        for census_row in census_rows:
            line_number = census_row.get("line") if isinstance(census_row, dict) else None
            cell = census_row.get("cell") if isinstance(census_row, dict) else None
            attempt = census_row.get("attempt") if isinstance(census_row, dict) else None
            if (
                isinstance(line_number, bool)
                or not isinstance(line_number, int)
                or not 1 <= line_number <= len(lines)
                or line_number in seen
                or not isinstance(cell, dict)
            ):
                raise EvidenceError(f"{run_id}: malformed census row in {path}: {census_row!r}")
            seen.add(line_number)
            try:
                result = json.loads(lines[line_number - 1])
            except ValueError as error:
                raise EvidenceError(
                    f"{run_id}: {path} line {line_number} is not JSON"
                ) from error
            if not isinstance(result, dict):
                raise EvidenceError(f"{run_id}: {path} line {line_number} is not an object")
            identity = tuple(cell.get(field) for field in CELL_FIELDS)
            if (
                tuple(result.get(field) for field in CELL_FIELDS) != identity
                or result.get("attempt") != attempt
            ):
                raise EvidenceError(
                    f"{run_id}: {path} line {line_number} does not match its census row"
                )
            if (
                identity[0] != lane
                or not isinstance(identity[1], str)
                or identity[1].replace("-", "_") != category_suffix
                or not isinstance(identity[2], str)
                or not isinstance(identity[3], str)
            ):
                raise EvidenceError(
                    f"{run_id}: {path} line {line_number} holds a cell of another bucket"
                )
            if cell_nodes.setdefault(identity, node) != node:
                raise EvidenceError(f"{run_id}: cell {cell_label(identity)} appears in two buckets")
            histories[identity].append((line_number, attempt, result.get("outcome"), result))
        summary_file = artifact.with_name("summary.json")
        summary = read_json(summary_file)
        if summary is None:
            raise EvidenceError(f"{run_id}: missing or unreadable {summary_file}")
        inapplicable = summary.get("host_inapplicable_cells", [])
        if not isinstance(inapplicable, list):
            raise EvidenceError(f"{run_id}: {summary_file} host_inapplicable_cells is not a list")
        buckets[node] = {
            "lane": lane,
            "category_suffix": category_suffix,
            "rows": len(census_rows),
            "host_inapplicable": inapplicable,
        }

    cells: dict[tuple, dict] = {}
    for identity, history in histories.items():
        history.sort(key=lambda item: item[0])
        verdict, error = cell_verdict([(attempt, outcome) for _, attempt, outcome, _ in history])
        if error is not None:
            raise EvidenceError(f"{run_id}: cell {cell_label(identity)}: {error}")
        selected = history[-1][3]
        cells[identity] = {
            "verdict": verdict,
            "node": cell_nodes[identity],
            "attempts": [str(outcome) for _, _, outcome, _ in history],
            "classification": selected.get("classification"),
            "duration_ms": sum(number(item[3].get("duration_ms")) or 0 for item in history),
            "cpu_usage_usec": sum(number(item[3].get("cpu_usage_usec")) or 0 for item in history),
            "source": "results",
            "error_kind": selected.get("error_kind"),
            "reason": selected.get("reason"),
        }
    # The runner may report a host-inapplicable cell only in summary.json.
    # Result rows decide; a summary cell is added only when no row exists.
    for node, info in buckets.items():
        for item in info["host_inapplicable"]:
            test = item.get("test") if isinstance(item, dict) else None
            mode = item.get("mode") if isinstance(item, dict) else None
            backend = item.get("backend") if isinstance(item, dict) else None
            if not isinstance(test, str) or not isinstance(mode, str) or not (
                backend is None or isinstance(backend, str)
            ):
                raise EvidenceError(f"{run_id}: malformed host-inapplicable cell in {node}: {item!r}")
            category = test.split("/", 1)[0]
            if category.replace("-", "_") != info["category_suffix"]:
                raise EvidenceError(
                    f"{run_id}: host-inapplicable cell {test} is not in bucket {node}"
                )
            identity = (info["lane"], category, test, mode, backend)
            if identity not in cells:
                cells[identity] = {
                    "verdict": "HOST-INAPPLICABLE",
                    "node": node,
                    "attempts": [],
                    "classification": None,
                    "duration_ms": 0,
                    "cpu_usage_usec": 0,
                    "source": "summary",
                    "reason": item.get("reason"),
                }
    return {"run_id": run_id, "cells": cells, "buckets": sorted(buckets)}


def infrastructure_errors(evidence: Mapping[str, object]) -> list[dict]:
    """Cells that ended in ERROR because the harness, not the test, failed.

    Hermit's E2E harness marks such a result row `error_kind: infrastructure`
    (for example, a cell refused before launch because its live CPU could not
    be measured). The cell never ran, so its verdict says nothing about Hermit.
    """

    cells: Mapping[tuple, dict] = evidence["cells"]  # type: ignore[assignment]
    return [
        {"cell": cell_label(identity), "node": cell["node"], "reason": str(cell.get("reason") or "")[:300]}
        for identity, cell in sorted(cells.items(), key=lambda item: sort_key(item[0]))
        if cell["verdict"] == "ERROR" and cell.get("error_kind") == "infrastructure"
    ]


def buck_infrastructure_errors(root: Path, row: Mapping[str, object]) -> list[dict]:
    """`infrastructure_errors` of one recorded run; [] when its evidence is unusable.

    Unusable evidence is not judged here: the run stays selectable and the
    comparison reports it as a non-verdict, as it did before this check.
    """

    try:
        return infrastructure_errors(load_run_evidence(root, row))
    except EvidenceError:
        return []


def listed(report: Mapping[str, object], key: str) -> list:
    value = report.get(key)
    return value if isinstance(value, list) else []


def infrastructure_text(broken: Sequence[Mapping[str, object]]) -> str:
    newest = broken[0]
    examples = newest.get("examples") or [{}]
    example = examples[0]  # type: ignore[index]
    text = (
        f"{newest['infrastructure_cells']} cell(s) of Buck run {newest['run_id']} "
        f"({short(newest['commit'])}) ended in an infrastructure ERROR and never ran "
        f"(e.g. {example.get('cell')}: {str(example.get('reason'))[:160]})"
    )
    if len(broken) > 1:
        text += f"; {len(broken) - 1} older Buck run(s) also broken"
    return text


def load_flaky_cells(path: Path = FLAKY_CELLS) -> tuple[frozenset[str], str | None]:
    value = read_json(path)
    if value is None or not isinstance(value.get("cells"), list):
        return frozenset(), f"known-flaky list {path} unreadable; cells not annotated"
    return (
        frozenset(
            str(item.get("cell"))
            for item in value["cells"]
            if isinstance(item, dict) and item.get("cell")
        ),
        None,
    )


# ---------------------------------------------------------------- compare


def node_table(row: Mapping[str, object]) -> dict[str, dict]:
    """Every node of one run under its Cargo name, with its status."""

    statuses: dict[str, str] = {}
    details: dict[str, dict] = {}

    def mark(name: object, status: str) -> None:
        if isinstance(name, str) and name:
            if STATUS_RANK[status] >= STATUS_RANK.get(statuses.get(name, ""), -1):
                statuses[name] = status

    for name in row.get("planned_nodes") or []:
        mark(name, "planned-not-run")
    for name in row.get("no_result_nodes") or []:
        mark(name, "no-result")
    for name in row.get("intentional_skipped_nodes") or []:
        mark(name, "intentional-skip")
    for name in row.get("dependency_skipped_nodes") or []:
        mark(name, "dependency-skipped")
    for gate in row.get("gates") or []:
        if not isinstance(gate, dict):
            continue
        name = gate.get("name")
        result = gate.get("result")
        if result in VERDICTS:
            mark(name, str(result))
        else:
            mark(name, "no-result")
        if isinstance(name, str):
            details[name] = {
                "complete": result in VERDICTS
                and not any(gate.get(key) for key in INCOMPLETE_GATE_KEYS),
                "real_seconds": number(gate.get("real_seconds")),
            }
    table: dict[str, dict] = {}
    for name, status in statuses.items():
        normalized = (
            name[: -len("_buck")]
            if name.endswith("_buck") and name[: -len("_buck")] not in statuses
            else name
        )
        detail = details.get(name, {})
        table[normalized] = {
            "name": name,
            "status": status,
            "complete": bool(detail.get("complete", False)),
            "real_seconds": detail.get("real_seconds"),
        }
    return table


def compare_nodes(buck: Mapping[str, dict], cargo: Mapping[str, dict]) -> dict:
    compared = 0
    mismatches: list[dict] = []
    one_verdict: list[dict] = []
    for node in sorted(set(buck) & set(cargo)):
        buck_status = buck[node]["status"]
        cargo_status = cargo[node]["status"]
        if buck_status in VERDICTS and cargo_status in VERDICTS:
            compared += 1
            if buck_status != cargo_status:
                mismatches.append(
                    {"node": node, "buck_name": buck[node]["name"], "buck": buck_status, "cargo": cargo_status}
                )
        elif buck_status in VERDICTS or cargo_status in VERDICTS:
            one_verdict.append({"node": node, "buck": buck_status, "cargo": cargo_status})

    def only(mine: Mapping[str, dict], other: Mapping[str, dict], expected: frozenset[str]) -> list[dict]:
        return [
            {"node": mine[node]["name"], "status": mine[node]["status"], "expected": node in expected}
            for node in sorted(set(mine) - set(other))
        ]

    return {
        "compared": compared,
        "mismatches": mismatches,
        "verdict_in_one_run_only": one_verdict,
        "buck_only": only(buck, cargo, EXPECTED_BUCK_ONLY_NODES),
        "cargo_only": only(cargo, buck, EXPECTED_CARGO_ONLY_NODES),
    }


def one_sided_reason(node: str, other_nodes: Mapping[str, dict], other: str) -> tuple[bool, str]:
    """(explained, reason) for a cell recorded by one run only."""

    entry = other_nodes.get(node)
    if entry is None:
        return False, f"{other} run has no {node} node"
    if entry["status"] not in VERDICTS:
        return True, f"{other} {entry['name']} {entry['status']}"
    if not entry["complete"]:
        return True, f"{other} {entry['name']} {entry['status']} before finishing (timed out, aborted or OOM)"
    return False, f"{other} {entry['name']} completed ({entry['status']}) without this cell"


def compare_cells(
    buck: Mapping[str, object],
    cargo: Mapping[str, object],
    buck_nodes: Mapping[str, dict],
    cargo_nodes: Mapping[str, dict],
    flaky: frozenset[str],
) -> dict:
    buck_cells: Mapping[tuple, dict] = buck["cells"]  # type: ignore[assignment]
    cargo_cells: Mapping[tuple, dict] = cargo["cells"]  # type: ignore[assignment]
    shared = sorted(set(buck_cells) & set(cargo_cells), key=sort_key)
    mismatches: list[dict] = []
    retry_differences: list[dict] = []
    for identity in shared:
        left, right = buck_cells[identity], cargo_cells[identity]
        if left["verdict"] != right["verdict"]:
            mismatches.append(
                {
                    "cell": cell_label(identity),
                    "node": left["node"],
                    "buck": left["verdict"],
                    "cargo": right["verdict"],
                    "buck_attempts": left["attempts"],
                    "cargo_attempts": right["attempts"],
                    "classification": right["classification"] or left["classification"],
                    "known_flaky": flaky_key(identity) in flaky,
                }
            )
        elif left["attempts"] != right["attempts"]:
            retry_differences.append(
                {
                    "cell": cell_label(identity),
                    "verdict": left["verdict"],
                    "buck_attempts": left["attempts"],
                    "cargo_attempts": right["attempts"],
                }
            )
    one_sided: list[dict] = []
    for side, mine, other_nodes, other in (
        ("buck", buck_cells, cargo_nodes, "cargo"),
        ("cargo", cargo_cells, buck_nodes, "buck"),
    ):
        missing = set(mine) - (set(buck_cells) & set(cargo_cells))
        for identity in sorted(missing, key=sort_key):
            explained, reason = one_sided_reason(mine[identity]["node"], other_nodes, other)
            one_sided.append(
                {
                    "cell": cell_label(identity),
                    "only_in": side,
                    "node": mine[identity]["node"],
                    "verdict": mine[identity]["verdict"],
                    "explained": explained,
                    "reason": reason,
                }
            )
    reasons = collections.Counter(
        (item["only_in"], item["explained"], item["reason"]) for item in one_sided
    )
    return {
        "buck_cells": len(buck_cells),
        "cargo_cells": len(cargo_cells),
        "compared": len(shared),
        "mismatches": mismatches,
        "retry_differences": retry_differences,
        "one_sided": one_sided,
        "one_sided_by_reason": [
            {"only_in": side, "explained": explained, "reason": reason, "cells": count}
            for (side, explained, reason), count in sorted(reasons.items(), key=lambda kv: (-kv[1], kv[0]))
        ],
    }


def run_timing(row: Mapping[str, object], table: Mapping[str, dict], evidence: Mapping[str, object] | None) -> dict:
    user = number(row.get("user_seconds"))
    system = number(row.get("sys_seconds"))
    cells = (evidence or {}).get("cells") or {}
    return {
        "real_seconds": number(row.get("real_seconds")),
        "user_seconds": user,
        "sys_seconds": system,
        "cpu_seconds": None if user is None or system is None else user + system,
        "e2e_bucket_real_seconds": sum(
            entry["real_seconds"] or 0
            for name, entry in table.items()
            if BUCKET_NODE_RE.fullmatch(name)
        ),
        "cell_wall_seconds": round(sum(cell["duration_ms"] for cell in cells.values()) / 1000, 1),
        "cell_cpu_seconds": round(sum(cell["cpu_usage_usec"] for cell in cells.values()) / 1e6, 1),
        "started_at": row.get("started_at"),
        "finished_at": row.get("finished_at"),
        "release_builder": row.get("release_builder"),
        "concurrent_validates": row.get("concurrent_validates"),
        "validation_complete": row.get("validation_complete"),
    }


def describe_run(row: Mapping[str, object]) -> dict:
    return {
        "run_id": row.get("run_id"),
        "runner": runner_of(row),
        "host": row.get("host"),
        "commit": row.get("commit"),
        "result": row.get("result"),
        "exit_code": row.get("exit_code"),
        "finished_at": row.get("finished_at"),
    }


def compare_runs(
    root: Path,
    buck_row: Mapping[str, object],
    cargo_row: Mapping[str, object],
    flaky: frozenset[str] = frozenset(),
) -> dict:
    """Compare one Buck run with one Cargo run; returns the comparison report part."""

    buck_nodes = node_table(buck_row)
    cargo_nodes = node_table(cargo_row)
    nodes = compare_nodes(buck_nodes, cargo_nodes)
    result: dict = {
        "buck": describe_run(buck_row),
        "cargo": describe_run(cargo_row),
        "overall": {"buck": buck_row.get("result"), "cargo": cargo_row.get("result")},
        "nodes": nodes,
        "cells": None,
        "reasons": [],
    }
    evidence: dict[str, dict | None] = {"buck": None, "cargo": None}
    if buck_row.get("commit") != cargo_row.get("commit"):
        result["outcome"] = "non-verdict"
        result["reasons"].append(
            f"the runs validated different commits ({short(buck_row.get('commit'))} vs "
            f"{short(cargo_row.get('commit'))})"
        )
    else:
        problems = []
        for side, row in (("buck", buck_row), ("cargo", cargo_row)):
            try:
                evidence[side] = load_run_evidence(root, row)
            except EvidenceError as error:
                problems.append(f"{side} cell evidence unusable: {error}")
        if problems:
            result["outcome"] = "non-verdict"
            result["reasons"].extend(problems)
        else:
            cells = compare_cells(evidence["buck"], evidence["cargo"], buck_nodes, cargo_nodes, flaky)
            result["cells"] = cells
            reasons = result["reasons"]
            if buck_row.get("result") != cargo_row.get("result"):
                reasons.append(
                    f"overall verdict buck {buck_row.get('result')} vs cargo {cargo_row.get('result')}"
                )
            if nodes["mismatches"]:
                reasons.append(
                    f"{len(nodes['mismatches'])} node verdict(s) differ: "
                    + ", ".join(
                        f"{item['node']} buck {item['buck']}/cargo {item['cargo']}"
                        for item in nodes["mismatches"][:LIST_LIMIT]
                    )
                )
            if cells["mismatches"]:
                reasons.append(
                    f"{len(cells['mismatches'])} cell verdict(s) differ: "
                    + ", ".join(
                        f"{item['cell']} buck {item['buck']}/cargo {item['cargo']}"
                        + (" (known flaky)" if item["known_flaky"] else "")
                        for item in cells["mismatches"][:LIST_LIMIT]
                    )
                )
            unexplained = [item for item in cells["one_sided"] if not item["explained"]]
            if unexplained:
                reasons.append(
                    f"{len(unexplained)} cell(s) missing from a completed bucket of the other run: "
                    + ", ".join(
                        f"{item['cell']} only in {item['only_in']}" for item in unexplained[:LIST_LIMIT]
                    )
                )
            if reasons:
                result["outcome"] = "mismatch"
            elif cells["compared"] == 0:
                result["outcome"] = "non-verdict"
                reasons.append("no cell was recorded by both runs")
            else:
                result["outcome"] = "match"
    result["timing"] = {
        "buck": run_timing(buck_row, buck_nodes, evidence["buck"]),
        "cargo": run_timing(cargo_row, cargo_nodes, evidence["cargo"]),
        "note": (
            "user/sys CPU is measured on this host only; CPU spent by Buck remote "
            "execution is not counted"
        ),
    }
    return result


def seconds_text(value: object) -> str:
    amount = number(value)
    return "?" if amount is None else f"{amount:.0f}"


def render_summary(report: Mapping[str, object]) -> str:
    """The one-line summary the health tick shows."""

    outcome = report.get("outcome")
    path = report.get("report_path") or "(not written)"
    comparison = report.get("comparison") or {}
    commit = short(report.get("commit"))
    broken = listed(report, "buck_infrastructure")
    if broken:
        # The broken run is its own finding and leads the line; what follows
        # is the comparison of the newest clean Buck run, if there was one.
        if outcome == "buck-infra-broken" and not comparison:
            reasons = "; ".join(str(reason) for reason in listed(report, "reasons"))
            return f"BUCK INFRA BROKEN: {infrastructure_text(broken)}; {reasons}; report {path}"
        rest = dict(report, buck_infrastructure=None)
        if outcome == "buck-infra-broken" and isinstance(comparison, dict):
            rest["outcome"] = comparison.get("outcome")
        joiner = "compared the newest clean Buck run instead: " if comparison else ""
        return f"BUCK INFRA BROKEN: {infrastructure_text(broken)}; {joiner}{render_summary(rest)}"
    if outcome == "buck-infra-broken":
        reasons = "; ".join(str(reason) for reason in listed(report, "reasons")) or "no reason recorded"
        return f"BUCK INFRA BROKEN: {reasons}; report {path}"
    if outcome == "match" and comparison:
        buck = comparison["buck"]
        cargo = comparison["cargo"]
        cells = comparison["cells"]
        timing = comparison["timing"]
        explained = sum(1 for item in cells["one_sided"] if item["explained"])
        return (
            f"MATCH {commit} ({buck['runner']} {buck['run_id']} vs cargo {cargo['run_id']}): "
            f"overall {buck['result']}={cargo['result']}; {comparison['nodes']['compared']} nodes and "
            f"{cells['compared']} cells agree; one-sided cells {explained} explained; "
            f"wall {seconds_text(timing['buck']['real_seconds'])} s vs "
            f"{seconds_text(timing['cargo']['real_seconds'])} s; local CPU "
            f"{seconds_text(timing['buck']['cpu_seconds'])} s vs "
            f"{seconds_text(timing['cargo']['cpu_seconds'])} s; report {path}"
        )
    reasons = "; ".join(str(reason) for reason in report.get("reasons") or []) or "no reason recorded"
    if outcome == "mismatch":
        return f"MISMATCH {commit}: {reasons}; report {path}"
    return f"NON-VERDICT: {reasons}; report {path}"


def render_text(report: Mapping[str, object]) -> str:
    lines = [str(report.get("summary") or render_summary(report)), ""]
    for item in listed(report, "buck_infrastructure"):
        lines.append(
            f"buck infrastructure broken: {item['run_id']} ({short(item['commit'])}): "
            f"{item['infrastructure_cells']} cell(s) ended in an infrastructure ERROR; skipped"
        )
        for example in item.get("examples") or []:
            lines.append(f"  {example['cell']} ({example['node']}): {example['reason']}")
    comparison = report.get("comparison") or {}
    for side in ("buck", "cargo"):
        run = comparison.get(side)
        if run:
            timing = comparison["timing"][side]
            lines.append(
                f"{side}: {run['run_id']} runner={run['runner']} result={run['result']} "
                f"exit={run['exit_code']} wall={seconds_text(timing['real_seconds'])}s "
                f"user={seconds_text(timing['user_seconds'])}s sys={seconds_text(timing['sys_seconds'])}s "
                f"e2e-buckets={seconds_text(timing['e2e_bucket_real_seconds'])}s "
                f"cell-cpu={seconds_text(timing['cell_cpu_seconds'])}s finished={run['finished_at']}"
            )
    if comparison.get("timing"):
        lines.append(f"note: {comparison['timing']['note']}")
    nodes = comparison.get("nodes")
    if nodes:
        lines.append(
            f"nodes: {nodes['compared']} compared, {len(nodes['mismatches'])} differ, "
            f"{len(nodes['verdict_in_one_run_only'])} with a verdict in one run only, "
            f"{len(nodes['buck_only'])} Buck-only, {len(nodes['cargo_only'])} Cargo-only"
        )
        for item in nodes["mismatches"]:
            lines.append(f"  node differs: {item['node']} buck={item['buck']} cargo={item['cargo']}")
        for item in nodes["verdict_in_one_run_only"][:20]:
            lines.append(f"  node verdict one side: {item['node']} buck={item['buck']} cargo={item['cargo']}")
        if len(nodes["verdict_in_one_run_only"]) > 20:
            lines.append(
                f"  ... {len(nodes['verdict_in_one_run_only']) - 20} more node(s) with a verdict "
                "in one run only (see report.json)"
            )
        for side in ("buck_only", "cargo_only"):
            unexpected = [item["node"] for item in nodes[side] if not item["expected"]]
            if unexpected:
                lines.append(f"  unexpected {side} nodes: {', '.join(unexpected)}")
    cells = comparison.get("cells")
    if cells:
        lines.append(
            f"cells: buck {cells['buck_cells']}, cargo {cells['cargo_cells']}, compared {cells['compared']}, "
            f"{len(cells['mismatches'])} differ, {len(cells['retry_differences'])} differ only in retries, "
            f"{len(cells['one_sided'])} one-sided"
        )
        for item in cells["mismatches"][:40]:
            lines.append(
                f"  cell differs: {item['cell']} buck={item['buck']}{item['buck_attempts']} "
                f"cargo={item['cargo']}{item['cargo_attempts']}"
                + (" known-flaky" if item["known_flaky"] else "")
            )
        if len(cells["mismatches"]) > 40:
            lines.append(f"  ... {len(cells['mismatches']) - 40} more differing cell(s) (see report.json)")
        for item in cells["one_sided_by_reason"]:
            lines.append(
                f"  {item['cells']} cell(s) only in {item['only_in']}: "
                f"{'explained' if item['explained'] else 'UNEXPLAINED'} ({item['reason']})"
            )
    for note in report.get("notes") or []:
        lines.append(f"note: {note}")
    return "\n".join(lines) + "\n"


def write_report(root: Path, report: dict, directory: Path | None = None) -> Path:
    if directory is None:
        directory = reports_root(root) / f"{report['stamp']}-{short(report.get('commit'))}"
    directory.mkdir(parents=True, exist_ok=True)
    report["report_path"] = str(directory / "report.json")
    report["summary"] = render_summary(report)
    write_json(directory / "report.json", report)
    (directory / "summary.txt").write_text(render_text(report), encoding="utf-8")
    return directory / "report.json"


# ---------------------------------------------------------------- worker


def load_ledger_rows(root: Path) -> list[dict]:
    sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "ledger"))
    import validate_rows  # noqa: E402

    return list(validate_rows.current_validation_run_rows(validate_rows.rows(root=root)))


def fetch_hermit_main(root: Path) -> str | None:
    """Refresh Hermit origin/main for the ancestry check; returns a problem or None."""

    with_proxy = shutil.which("with-proxy")
    if with_proxy is None:
        return "with-proxy unavailable; Hermit origin/main not refreshed"
    try:
        completed = subprocess.run(
            [with_proxy, "git", "-C", str(root / "hermit"), "fetch", "--no-tags", "origin", "main"],
            capture_output=True,
            text=True,
            check=False,
            timeout=FETCH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return f"Hermit main fetch exceeded {FETCH_TIMEOUT_SECONDS} s; origin/main may be stale"
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip().splitlines()
        return f"Hermit main fetch failed ({detail[-1] if detail else completed.returncode}); origin/main may be stale"
    return None


def hermit_main_contains(root: Path, commit: str) -> bool | None:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root / "hermit"), "merge-base", "--is-ancestor", commit, "origin/main"],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )
    except subprocess.TimeoutExpired:
        return None
    if completed.returncode == 0:
        return True
    if completed.returncode == 1:
        return False
    return None


def run_validate_process(command: Sequence[str], cwd: Path, timeout: float) -> subprocess.CompletedProcess:
    return subprocess.run(
        list(command), cwd=cwd, capture_output=True, text=True, check=False, timeout=timeout
    )


@dataclasses.dataclass
class Dependencies:
    load_rows: Callable[[Path], list[dict]] = load_ledger_rows
    fetch_main: Callable[[Path], str | None] = fetch_hermit_main
    on_main: Callable[[Path, str], bool | None] = hermit_main_contains
    run_validate: Callable[[Sequence[str], Path, float], subprocess.CompletedProcess] = run_validate_process
    hostname: Callable[[], str] = lambda: socket.gethostname().split(".")[0]
    now: Callable[[], dt.datetime] = lambda: dt.datetime.now(dt.timezone.utc)
    sleep: Callable[[float], None] = time.sleep
    environment: Mapping[str, str] = dataclasses.field(default_factory=lambda: dict(os.environ))
    flaky_path: Path = FLAKY_CELLS


def validate_command(root: Path, commit: str, unit: str, environment: Mapping[str, str]) -> list[str]:
    """validate-run of `commit` with the Cargo E2E runner, through the verified tool.

    Same route as the integration-tip launcher: the installed operational tool
    materializes the exact parent tool SHA and runs its ci-hub, so the run
    never uses the shared parent checkout's ci-hub. `--skip-if-recorded` is
    deliberately absent (the Buck row is already a full record of this
    commit), and so are `--frozen-validate` and `--no-wait`.
    """

    tool = environment.get("DEV_HERMIT_OPERATIONAL_TOOL", "")
    parent_sha = environment.get("DEV_HERMIT_TOOL_PARENT_SHA", "")
    if (
        not tool
        or not os.path.isabs(tool)
        or not os.path.isfile(tool)
        or not os.access(tool, os.X_OK)
        or SHA_RE.fullmatch(parent_sha) is None
    ):
        raise ValueError(
            "operational-tool handoff unavailable; Cargo validate not launched "
            "(validate-run must run from the verified current tool)"
        )
    return [
        tool,
        "--run-current-tool",
        "--state-root",
        str(root),
        "--tool-parent-sha",
        parent_sha,
        "--tool-relative",
        "ci-hub/ci-hub",
        "--",
        "validate-run",
        "--checkout",
        str(root / "hermit"),
        "--state-root",
        str(root),
        "--repo",
        REPO,
        "--materialize-target",
        "--agent",
        AGENT,
        "--target",
        commit,
        "--unit",
        unit,
        "--wait",
        str(VALIDATE_WAIT_SECONDS),
        "--admit-main-ancestor",
        "--json",
        "--",
        "full",
        "--e2e-runner",
        "cargo",
    ]


def tail(text: object, limit: int = 4000) -> str:
    return text[-limit:] if isinstance(text, str) else ""


def obtain_comparison(root: Path, report: dict, deps: Dependencies) -> None:
    """Fill `report` with the selection, any Cargo launch, and the comparison."""

    host = deps.hostname()
    report["host"] = host
    rows = deps.load_rows(root)
    fetch_problem = deps.fetch_main(root)
    if fetch_problem:
        report["notes"].append(fetch_problem)
    buck, cargo, skipped = select_buck_run(
        rows,
        host,
        lambda sha: deps.on_main(root, sha),
        infrastructure_errors=lambda row: buck_infrastructure_errors(root, row),
    )
    report["selection"] = {"skipped": skipped}
    broken = [item for item in skipped if "infrastructure_cells" in item]
    if broken:
        report["buck_infrastructure"] = broken
    if buck is None:
        if broken:
            report["outcome"] = "buck-infra-broken"
            report["reasons"].append(
                f"no Buck-runner full validation of {REPO} on {host} ran its cells "
                f"({len(skipped)} candidate(s) skipped), so nothing was compared"
            )
            return
        report["outcome"] = "non-verdict"
        report["reasons"].append(
            f"no completed Buck-runner full validation of {REPO} on {host}"
            + (f" on Hermit main ({len(skipped)} newer candidate(s) skipped)" if skipped else "")
        )
        return
    commit = str(buck["commit"])
    report["commit"] = commit
    report["selection"]["buck_run"] = buck["run_id"]
    if cargo is not None:
        report["selection"]["cargo"] = "reused an existing Cargo-runner full validate"
    else:
        unit = f"validate-{AGENT}-{commit[:12]}-{report['stamp']}"
        try:
            command = validate_command(root, commit, unit, deps.environment)
        except ValueError as error:
            report["outcome"] = "non-verdict"
            report["reasons"].append(
                f"no completed Cargo-runner full validate of {commit[:12]} on {host} and {error}"
            )
            return
        launch: dict = {"unit": unit, "command": command, "started_at": utc_now()}
        report["validate_launch"] = launch
        report["selection"]["cargo"] = f"launched {unit}"
        print(f"{utc_now()} launching Cargo validate {unit} of {commit}", flush=True)
        del rows
        try:
            completed = deps.run_validate(command, root, VALIDATE_PROCESS_TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            launch["finished_at"] = utc_now()
            report["outcome"] = "non-verdict"
            report["reasons"].append(
                f"Cargo validate {unit} waiter exceeded {VALIDATE_PROCESS_TIMEOUT_SECONDS} s; "
                "the next nightly run compares it if it completes"
            )
            return
        launch.update(
            {
                "finished_at": utc_now(),
                "returncode": completed.returncode,
                "stdout_tail": tail(completed.stdout),
                "stderr_tail": tail(completed.stderr),
            }
        )
        print(f"{utc_now()} Cargo validate {unit} returned {completed.returncode}", flush=True)
        row = None
        for attempt in range(LEDGER_POLL_ATTEMPTS):
            row = next(
                (item for item in deps.load_rows(root) if isinstance(item, dict) and item.get("run_id") == unit),
                None,
            )
            if row is not None:
                break
            if attempt + 1 < LEDGER_POLL_ATTEMPTS:
                deps.sleep(LEDGER_POLL_SECONDS)
        if row is None:
            report["outcome"] = "non-verdict"
            report["reasons"].append(
                f"Cargo validate {unit} (exit {completed.returncode}) left no ledger row"
            )
            return
        if not completed_full(row, host) or runner_of(row) != "cargo" or row.get("commit") != commit:
            report["outcome"] = "non-verdict"
            report["reasons"].append(
                f"Cargo validate {unit} (exit {completed.returncode}) did not record a completed "
                f"Cargo-runner full validate of {commit[:12]} (result {row.get('result')}, "
                f"runner {runner_of(row)}, interruption {row.get('interruption_signal')})"
            )
            return
        cargo = row
    flaky, flaky_problem = load_flaky_cells(deps.flaky_path)
    if flaky_problem:
        report["notes"].append(flaky_problem)
    comparison = compare_runs(root, buck, cargo, flaky)
    report["comparison"] = comparison
    report["outcome"] = comparison["outcome"]
    report["reasons"].extend(comparison["reasons"])
    # An older clean run matching does not make the newest Buck run's broken
    # infrastructure a match: it is its own finding.
    if broken and comparison["outcome"] == "match":
        report["outcome"] = "buck-infra-broken"


def new_report(deps: Dependencies, launch_id: str | None, kind: str) -> dict:
    moment = deps.now()
    return {
        "schema": 1,
        "kind": kind,
        "launch_id": launch_id,
        "generated_at": moment.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "stamp": compact_stamp(moment),
        "host": None,
        "commit": None,
        "outcome": "non-verdict",
        "reasons": [],
        "notes": [],
        "selection": None,
        "validate_launch": None,
        "comparison": None,
    }


def run_worker(root: Path, launch_id: str, deps: Dependencies | None = None) -> int:
    deps = deps or Dependencies()
    report = new_report(deps, launch_id, "buck-cargo-repro")
    try:
        obtain_comparison(root, report, deps)
    except Exception as error:  # noqa: BLE001 - any failure is a non-verdict, never a match
        report["outcome"] = "non-verdict"
        report["reasons"].append(f"worker error: {type(error).__name__}: {error}")
        report["traceback"] = traceback.format_exc()[-4000:]
    path = write_report(root, report)
    comparison = report.get("comparison") or {}
    write_json(
        state_path(root),
        {
            "launch_id": launch_id,
            "finished_at": utc_now(),
            "outcome": report["outcome"],
            "summary": report["summary"],
            "report": str(path),
            "commit": report.get("commit"),
            "run_ids": {
                "buck": (comparison.get("buck") or {}).get("run_id"),
                "cargo": (comparison.get("cargo") or {}).get("run_id"),
            },
        },
    )
    print(f"{utc_now()} {report['summary']}", flush=True)
    return 0 if report["outcome"] == "match" else 1


def compare_recorded(
    root: Path,
    buck_run: str,
    cargo_run: str,
    output: Path | None = None,
    deps: Dependencies | None = None,
) -> int:
    """Operator command: compare two recorded runs by id; never launches."""

    deps = deps or Dependencies()
    report = new_report(deps, None, "buck-cargo-repro-compare")
    try:
        rows = {
            item.get("run_id"): item
            for item in deps.load_rows(root)
            if isinstance(item, dict) and item.get("run_id") in (buck_run, cargo_run)
        }
        missing = [
            run_id
            for run_id in (buck_run, cargo_run)
            if not completed_full(rows.get(run_id), None)
        ]
        if missing:
            report["reasons"].append(
                "not a completed full Hermit validate in the ledger: " + ", ".join(missing)
            )
        else:
            report["commit"] = rows[buck_run].get("commit")
            flaky, flaky_problem = load_flaky_cells(deps.flaky_path)
            if flaky_problem:
                report["notes"].append(flaky_problem)
            comparison = compare_runs(root, rows[buck_run], rows[cargo_run], flaky)
            report["comparison"] = comparison
            report["outcome"] = comparison["outcome"]
            report["reasons"].extend(comparison["reasons"])
    except Exception as error:  # noqa: BLE001
        report["outcome"] = "non-verdict"
        report["reasons"].append(f"compare error: {type(error).__name__}: {error}")
    if output is None:
        output = reports_root(root) / f"{report['stamp']}-compare-{short(report.get('commit'))}"
    write_report(root, report, output)
    print(render_text(report), end="")
    return 0 if report["outcome"] == "match" else 1


# ---------------------------------------------------------------- gate


def status(root: Path) -> int:
    """Read-only: print the newest launch and its result; 0 only for a match."""

    record = read_json(launch_record_path(root))
    state = read_json(state_path(root))
    if record is None:
        emit("none", f"no nightly Buck-vs-Cargo repro has been launched from {root}")
        return 1
    if state is None or state.get("launch_id") != record.get("launch_id"):
        emit(
            "pending",
            f"repro {record.get('launch_id')} started {record.get('started_at')} has no result yet "
            f"(log {log_path(root)})",
        )
        return 1
    emit(str(state.get("outcome")), str(state.get("summary")))
    return 0 if state.get("outcome") == "match" else 1


def report_result(
    root: Path,
    *,
    run: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    which: Callable[[str], str | None] = shutil.which,
) -> int:
    """Health-tick gate run on EVERY tick: report the newest finished result.

    `launch` runs once a day, so the result it reports is one night old, and
    a tick that skips a gate lets tick-hub resolve that gate's warning. This
    gate reads only the state file and the unit state, so it can run on every
    tick: a result reaches tick-hub at the first tick after the worker writes
    it, and the warning stays raised for as long as the newest result is not
    a match.

    Exit 0: nothing launched yet, a match, or a run in progress after a match.
    Exit 1: the newest result is not a match (also while the next run is in
    progress), or the newest launch ended without a result.
    Exit 75: the unit state could not be read.
    """

    record = read_json(launch_record_path(root))
    state = read_json(state_path(root))
    if record is None:
        emit("none", f"no nightly Buck-vs-Cargo repro has been launched from {root}")
        return 0
    if state is not None and state.get("launch_id") == record.get("launch_id"):
        emit(str(state.get("outcome")), str(state.get("summary")))
        return 0 if state.get("outcome") == "match" else 1
    systemctl = which("systemctl")
    if systemctl is None:
        emit("unknown", "systemctl unavailable; cannot tell whether the Buck-vs-Cargo repro is running")
        return 75
    active = run(
        [systemctl, "--user", "is-active", "--quiet", f"{UNIT}.service"],
        capture_output=True,
        text=True,
        check=False,
        timeout=10,
    )
    if active.returncode == 0:
        running = f"repro {record.get('launch_id')} running since {record.get('started_at')}"
        if state is not None and state.get("outcome") != "match":
            emit(str(state.get("outcome")), f"{state.get('summary')}; {running}")
            return 1
        emit("running", running)
        return 0
    if active.returncode not in (3, 4):
        detail = (active.stderr or active.stdout or "").strip() or f"exit {active.returncode}"
        emit("unknown", f"Buck-vs-Cargo repro unit state unreadable: {detail}")
        return 75
    emit(
        "attention",
        f"repro {record.get('launch_id')} started {record.get('started_at')} ended without "
        f"a result (see {log_path(root)})",
    )
    return 1


def previous_problem(root: Path) -> str | None:
    """The previous launch's problem, or None when it matched or none exists."""

    record = read_json(launch_record_path(root))
    if record is None:
        return None
    state = read_json(state_path(root))
    if state is None or state.get("launch_id") != record.get("launch_id"):
        return (
            f"previous repro {record.get('launch_id')} started {record.get('started_at')} "
            f"left no result (see {log_path(root)})"
        )
    if state.get("outcome") != "match":
        return str(state.get("summary") or f"previous repro outcome {state.get('outcome')}")
    return None


def launch_command(
    root: Path,
    systemd_run: str,
    launch_id: str,
    environment: Mapping[str, str] | None = None,
) -> list[str]:
    environment = os.environ if environment is None else environment
    operational_tool = environment.get("DEV_HERMIT_OPERATIONAL_TOOL", "")
    parent_sha = environment.get("DEV_HERMIT_TOOL_PARENT_SHA", "")
    cache_root = environment.get("DEV_HERMIT_TOOL_CACHE", "")
    if operational_tool or parent_sha or cache_root:
        if (
            not operational_tool
            or not os.path.isabs(operational_tool)
            or not os.path.isfile(operational_tool)
            or not os.access(operational_tool, os.X_OK)
            or SHA_RE.fullmatch(parent_sha) is None
            or not cache_root
            or not os.path.isabs(cache_root)
            or not os.path.isdir(cache_root)
        ):
            raise ValueError(
                "incomplete operational-tool handoff; nightly Buck-vs-Cargo repro not launched"
            )
        working_directory = root
        worker = [
            operational_tool,
            "--run-current-tool",
            "--state-root",
            str(root),
            "--cache-root",
            cache_root,
            "--tool-parent-sha",
            parent_sha,
            "--tool-relative",
            "ci-hub/series/buck_cargo_repro.py",
            "--",
        ]
    else:
        working_directory = SOURCE_ROOT
        worker = [sys.executable, str(SOURCE_ROOT / "ci-hub" / "series" / "buck_cargo_repro.py")]
    log = log_path(root)
    override = environment.get(CPU_PLACEMENT_OVERRIDE_ENV)
    return [
        systemd_run,
        "--user",
        "--quiet",
        "--collect",
        f"--unit={UNIT}",
        "--description=nightly Cargo-runner repro of the newest Buck-runner Hermit validate",
        f"--property=RuntimeMaxSec={UNIT_RUNTIME_SECONDS}",
        f"--property=StandardOutput=append:{log}",
        f"--property=StandardError=append:{log}",
        f"--working-directory={working_directory}",
        f"--setenv=PATH={environment.get('PATH', '')}",
        "--setenv=PYTHONUNBUFFERED=1",
        f"--setenv=DEV_HERMIT_PARENT={root}",
        f"--setenv=DEV_HERMIT_TOOL_ROOT={SOURCE_ROOT}",
        *(f"--setenv={name}={value}" for name, value in sorted(UNIT_GIT_SETENV.items())),
        # validate-run derives its own CPU placement; only an explicit
        # override is forwarded, as the integration-tip launcher does.
        *([f"--setenv={CPU_PLACEMENT_OVERRIDE_ENV}={override}"] if override else []),
        "--",
        *worker,
        "_run",
        "--launch-id",
        launch_id,
        "--state-root",
        str(root),
    ]


def launch(
    root: Path,
    *,
    run: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    which: Callable[[str], str | None] = shutil.which,
    environment: Mapping[str, str] | None = None,
    now: Callable[[], dt.datetime] = lambda: dt.datetime.now(dt.timezone.utc),
) -> int:
    """Health-tick gate: report the previous result and start one nightly run."""

    holds, problems = measurement_hold.active_holds(root)
    if holds:
        emit("held", measurement_hold.hold_text(holds) + "; nightly Buck-vs-Cargo repro not launched")
        return 0
    if problems:
        print(f"hold_problems={measurement_hold.problem_text(problems)}")
    systemctl = which("systemctl")
    systemd_run = which("systemd-run")
    missing = [name for name, value in (("systemctl", systemctl), ("systemd-run", systemd_run)) if value is None]
    if missing:
        emit("unavailable", "nightly Buck-vs-Cargo repro not launched; missing " + ",".join(missing))
        return 1
    lock = launch_lock_path(root)
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a+") as handle:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            emit("running", f"nightly Buck-vs-Cargo repro launch already in progress as {UNIT}")
            return 0
        active = run(
            [systemctl, "--user", "is-active", "--quiet", f"{UNIT}.service"],
            capture_output=True,
            text=True,
            check=False,
            timeout=10,
        )
        if active.returncode == 0:
            emit("running", f"nightly Buck-vs-Cargo repro still active as {UNIT}")
            return 0
        if active.returncode not in (3, 4):
            detail = (active.stderr or active.stdout or "").strip() or f"exit {active.returncode}"
            emit("unavailable", f"nightly Buck-vs-Cargo repro unit state unreadable: {detail}")
            return 1
        problem = previous_problem(root)
        prefix = f"{problem}; " if problem else ""
        launch_id = f"{compact_stamp(now())}-{secrets.token_hex(4)}"
        try:
            command = launch_command(root, systemd_run, launch_id, environment)
        except ValueError as error:
            emit("refused", f"{prefix}{error}")
            return 1
        log = log_path(root)
        with log.open("a", encoding="utf-8") as retained:
            print(f"{utc_now()} buck_cargo_repro: launching {launch_id}", file=retained)
        started = run(command, capture_output=True, text=True, check=False, timeout=20)
        if started.returncode != 0:
            detail = (started.stderr or started.stdout or "").strip() or f"exit {started.returncode}"
            emit("unavailable", f"{prefix}systemd-run did not start {UNIT}: {detail}")
            return 1
        try:
            write_json(
                launch_record_path(root),
                {"launch_id": launch_id, "started_at": utc_now(), "unit": f"{UNIT}.service"},
            )
        except OSError as error:
            emit("unavailable", f"{prefix}started {UNIT} but could not record launch {launch_id}: {error}")
            return 1
    if problem:
        emit("attention", f"{problem}; relaunched as {launch_id}")
        return 1
    emit("launched", f"nightly Buck-vs-Cargo repro started as {UNIT} ({launch_id})")
    return 0


# ---------------------------------------------------------------- CLI


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    if list(argv[:1]) == ["_run"]:
        worker = argparse.ArgumentParser(prog="buck_cargo_repro.py _run")
        worker.add_argument("--launch-id", required=True)
        worker.add_argument("--state-root", type=Path, default=STATE_ROOT)
        arguments = worker.parse_args(list(argv[1:]))
        arguments.command = "_run"
        return arguments
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)
    start = commands.add_parser("launch", help="health-tick gate: report the last result, start the next run")
    start.add_argument("--state-root", type=Path, default=STATE_ROOT)
    show = commands.add_parser("status", help="print the newest launch and its result (read-only)")
    show.add_argument("--state-root", type=Path, default=STATE_ROOT)
    result = commands.add_parser(
        "report",
        help="health-tick gate for every tick: fail while the newest finished result is not a match",
    )
    result.add_argument("--state-root", type=Path, default=STATE_ROOT)
    compare = commands.add_parser("compare", help="compare two recorded runs by ledger run id")
    compare.add_argument("--state-root", type=Path, default=STATE_ROOT)
    compare.add_argument("--buck-run", required=True)
    compare.add_argument("--cargo-run", required=True)
    compare.add_argument("--output", type=Path, help="report directory (default under ignored/ci-hub/buck-cargo-repro)")
    return parser.parse_args(list(argv))


def main(argv: Sequence[str] | None = None) -> int:
    arguments = parse_args(sys.argv[1:] if argv is None else argv)
    root = arguments.state_root.resolve()
    if arguments.command == "_run":
        return run_worker(root, arguments.launch_id)
    if arguments.command == "launch":
        return launch(root)
    if arguments.command == "status":
        return status(root)
    if arguments.command == "report":
        return report_result(root)
    return compare_recorded(root, arguments.buck_run, arguments.cargo_run, arguments.output)


if __name__ == "__main__":
    sys.exit(main())

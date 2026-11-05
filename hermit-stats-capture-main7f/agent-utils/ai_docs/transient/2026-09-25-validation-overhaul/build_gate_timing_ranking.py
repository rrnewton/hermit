#!/usr/bin/env python3
"""Build a complete outer-validation-gate cost ranking from one dagrun profile.

The report is deliberately gate-level. Pytest's separate case-level ranking remains useful for
finding expensive Python cases, while this report covers every Rust, cross-language, packaging,
browser, example, and application gate that the canonical validation DAG executes.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import math
import os
import re
import subprocess
import tempfile
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Mapping, Sequence


AUDIT_DIR = Path(__file__).resolve().parent
REPO_ROOT = AUDIT_DIR.parents[2]
GENERATED_EVIDENCE_PATHS = frozenset(
    {
        (AUDIT_DIR / "gate-timing-provenance.json").relative_to(REPO_ROOT).as_posix(),
        (AUDIT_DIR / "gate-timing-ranking.tsv").relative_to(REPO_ROOT).as_posix(),
        (AUDIT_DIR / "test-timing-provenance.json").relative_to(REPO_ROOT).as_posix(),
        (AUDIT_DIR / "test-timing-ranking.tsv").relative_to(REPO_ROOT).as_posix(),
    }
)


@dataclass(frozen=True)
class Gate:
    """The public identity and rationale emitted by ``dagrun list``."""

    tag: str
    desc: str


PROFILE_COLUMNS = {
    "timestamp",
    "machine_id",
    "container_class",
    "run_id",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
    "step",
    "elapsed_s",
    "returncode",
    "ok",
    "timed_out",
    "cpu_timed_out",
    "peak_bytes",
    "memory_max_bytes",
    "memory_events_low",
    "memory_events_high",
    "memory_events_max",
    "memory_events_oom",
    "memory_events_oom_kill",
    "oom_kills",
    "memory_events_oom_group_kill",
    "started_offset_s",
    "finished_offset_s",
    "user_s",
    "sys_s",
}

RUN_METADATA_COLUMNS = (
    "timestamp",
    "machine_id",
    "container_class",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
)

BOOLEAN_PROOF_COLUMNS = ("ok", "timed_out", "cpu_timed_out")
INTEGER_PROOF_COLUMNS = (
    "returncode",
    "oom_kills",
    "peak_bytes",
    "memory_events_low",
    "memory_events_high",
    "memory_events_max",
    "memory_events_oom",
    "memory_events_oom_kill",
    "memory_events_oom_group_kill",
)
NONNEGATIVE_INTEGER_PROOF_COLUMNS = set(INTEGER_PROOF_COLUMNS) - {"returncode"}
FLOAT_PROOF_COLUMNS = (
    "elapsed_s",
    "started_offset_s",
    "finished_offset_s",
    "user_s",
    "sys_s",
)

RATIONALE_OVERRIDES = {
    "cross.dagrun.differential": (
        "Python/Rust parity across dagrun's observable command surface"
    ),
    "vibe-talk.shots.screenshots": (
        "real-browser rendering and interaction coverage that unit tests cannot provide"
    ),
    "cross.agentctl.differential": (
        "Python/Rust parity for agent lifecycle and remote-control behavior"
    ),
    "rust.dagrun.test": (
        "scheduler, containment, timeout, and process-tree integration behavior"
    ),
    "examples.run.python": "shipped examples behave as documented with the Python engine",
    "examples.run.rust": "shipped examples behave as documented with the Rust engine",
    "python.repository-infrastructure.test": (
        "fail-closed repository routing, packaging, and launcher infrastructure"
    ),
}

RECOMMENDATION_OVERRIDES = {
    "cross.dagrun.differential": (
        "retain; dominant critical path, so split or parallelize before adding cases"
    ),
    "vibe-talk.shots.screenshots": (
        "retain selected/browser coverage; profile and shard before widening"
    ),
    "cross.agentctl.differential": (
        "retain cross-runtime contract; reduce repeated process startup if optimizing"
    ),
    "rust.dagrun.test": (
        "retain process-safety coverage; shard isolated integration tests if stable"
    ),
    "examples.run.python": "retain user-facing contract; keep component-selective",
    "examples.run.rust": "retain user-facing contract; keep component-selective",
    "python.repository-infrastructure.test": (
        "retain fail-closed coverage; shard the component if it stays above two minutes"
    ),
}


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile-csv", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--dag", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--provenance-output", type=Path, required=True)
    return parser


def _required(row: Mapping[str, str], key: str) -> str:
    value = (row.get(key) or "").strip()
    if not value:
        raise ValueError(f"{key} is blank")
    return value


def _float(row: Mapping[str, str], key: str) -> float:
    value = float(_required(row, key))
    if not math.isfinite(value):
        raise ValueError(f"{key} is not finite")
    return value


def _int(row: Mapping[str, str], key: str) -> int:
    return int(_required(row, key))


def _bool(row: Mapping[str, str], key: str) -> bool:
    value = _required(row, key).lower()
    if value in {"true", "1", "yes"}:
        return True
    if value in {"false", "0", "no"}:
        return False
    raise ValueError(f"{key} is not a boolean")


def _coverage_rationale(step: Gate) -> str:
    return RATIONALE_OVERRIDES.get(step.tag, step.desc.rstrip(".") or step.tag)


def _recommendation(step: Gate, wall_s: float) -> str:
    overridden = RECOMMENDATION_OVERRIDES.get(step.tag)
    if overridden is not None:
        return overridden
    if step.tag.startswith("python-lifecycle."):
        return "retain destructive-safety coverage in isolated, width-capped shards"
    if step.tag.startswith("cross."):
        return "retain cross-runtime contract; keep component-selective"
    if step.tag.startswith(("packages.", "rust-packages.")):
        return "retain installability contract; keep component-selective"
    if step.tag.startswith("timeline.browser."):
        return "retain real-browser contract; keep browser-selective"
    if step.tag.startswith("vibe-talk."):
        return "retain application contract; keep vibe-talk-selective"
    if wall_s >= 120.0:
        return "retain; profile or shard before expanding this gate"
    if wall_s >= 30.0:
        return "retain; keep component-selective"
    return "retain; low measured wall-clock cost"


def _mib(value: int) -> str:
    return f"{value / 1048576:.2f}"


def _stage_bytes(path: Path, payload: bytes) -> Path:
    """Write and fsync a replacement beside ``path`` without publishing it yet."""

    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        mode = (path.stat().st_mode & 0o777) if path.exists() else 0o644
        os.fchmod(descriptor, mode)
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    return temporary


def _load_rows(path: Path, run_id: str) -> tuple[list[dict[str, str]], list[str]]:
    with path.open(newline="", encoding="utf-8") as handle:
        reader = csv.DictReader(handle)
        columns = list(reader.fieldnames or ())
        missing = sorted(PROFILE_COLUMNS - set(columns))
        if missing:
            raise SystemExit(f"profile is missing required columns: {missing}")
        rows = [
            {key: value or "" for key, value in row.items() if key is not None}
            for row in reader
            if row.get("run_id") == run_id
        ]
    if not rows:
        raise SystemExit(f"profile contains no rows for run_id={run_id!r}")
    return rows, columns


def _validate_run_rows(rows: Sequence[Mapping[str, str]]) -> dict[str, str]:
    """Reject unknown proof values and mixed run-level provenance.

    Blank is not zero: an absent exit status, timeout flag, OOM counter, or resource measurement
    is missing evidence and cannot support a clean-green claim. Run-level fields are batch
    metadata and therefore must have exactly one nonempty value across every selected row.
    """

    metadata: dict[str, str] = {}
    for key in RUN_METADATA_COLUMNS:
        try:
            values = {_required(row, key) for row in rows}
        except ValueError as error:
            raise SystemExit(f"run metadata is incomplete: {error}") from error
        if len(values) != 1:
            raise SystemExit(f"run metadata is inconsistent for {key}: {sorted(values)!r}")
        metadata[key] = values.pop()

    source_sha = metadata["git_sha"].lower()
    if re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", source_sha) is None:
        raise SystemExit(f"run git_sha is not one complete hexadecimal object ID: {source_sha!r}")
    try:
        if int(metadata["outer_jobs"]) <= 0:
            raise ValueError("must be positive")
    except ValueError as error:
        raise SystemExit(f"run outer_jobs is invalid: {metadata['outer_jobs']!r}") from error
    try:
        timestamp = datetime.fromisoformat(metadata["timestamp"].replace("Z", "+00:00"))
    except ValueError as error:
        raise SystemExit(f"run timestamp is invalid: {metadata['timestamp']!r}") from error
    if timestamp.tzinfo is None:
        raise SystemExit(f"run timestamp has no timezone: {metadata['timestamp']!r}")

    for row in rows:
        tag = (row.get("step") or "<blank step>").strip()
        try:
            for key in BOOLEAN_PROOF_COLUMNS:
                _bool(row, key)
            for key in INTEGER_PROOF_COLUMNS:
                value = _int(row, key)
                if key in NONNEGATIVE_INTEGER_PROOF_COLUMNS and value < 0:
                    raise ValueError(f"{key} is negative")
            for key in FLOAT_PROOF_COLUMNS:
                if _float(row, key) < 0:
                    raise ValueError(f"{key} is negative")
            memory_max = _required(row, "memory_max_bytes")
            if memory_max != "max" and int(memory_max) < 0:
                raise ValueError("memory_max_bytes is negative")
            if _float(row, "finished_offset_s") < _float(row, "started_offset_s"):
                raise ValueError("finished_offset_s precedes started_offset_s")
        except (TypeError, ValueError) as error:
            raise SystemExit(f"profile proof row for {tag!r} is invalid: {error}") from error
    return metadata


def _assert_current_clean_source(profile_git_sha: str) -> str:
    """Bind evidence inputs to HEAD, allowing only generated reports to differ."""

    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    if head.lower() != profile_git_sha.lower():
        raise SystemExit(
            "profile source does not match current HEAD: "
            f"profile={profile_git_sha}, current={head}"
        )
    diff = subprocess.run(
        [
            "git",
            "diff",
            "--quiet",
            "HEAD",
            "--",
            ".",
            *(f":(exclude){path}" for path in sorted(GENERATED_EVIDENCE_PATHS)),
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if diff.returncode not in (0, 1):
        raise SystemExit(f"cannot inspect source worktree: {diff.stderr.strip()}")
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.splitlines()
    unexpected_untracked = sorted(set(untracked) - GENERATED_EVIDENCE_PATHS)
    if diff.returncode != 0 or unexpected_untracked:
        raise SystemExit(
            "gate evidence inputs must match the profiled clean HEAD; "
            f"unexpected_untracked={unexpected_untracked}"
        )
    return head


def _dagrun_output(subcommand: str, dag_path: Path) -> str:
    completed = subprocess.run(
        [str(REPO_ROOT / "common/bin/dagrun"), subcommand, "--dag", str(dag_path)],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise SystemExit(
            f"cannot load canonical DAG with dagrun {subcommand}:\n{completed.stderr.strip()}"
        )
    if not completed.stdout:
        raise SystemExit(f"dagrun {subcommand} emitted no canonical DAG output")
    return completed.stdout


def _load_gates(dag_path: Path) -> tuple[dict[str, Gate], str, str]:
    """Ask the public loader for the canonical flattened graph.

    Using ``dagrun list`` here avoids growing a second include implementation inside an audit
    helper. If namespace or include semantics change, the ranking follows the same loader users
    and validation use.
    """

    gate_listing = _dagrun_output("list", dag_path)
    canonical_json = _dagrun_output("json", dag_path)
    try:
        json.loads(canonical_json)
    except json.JSONDecodeError as error:
        raise SystemExit(f"dagrun json emitted invalid JSON: {error}") from error
    gates: dict[str, Gate] = {}
    for line in gate_listing.splitlines():
        match = re.fullmatch(r"(\S+)\s+\[[^]]+\]\s+(.*?)(?:\s+<-\s+.*)?", line)
        if match is None:
            raise SystemExit(f"cannot parse dagrun list line: {line!r}")
        tag, desc = match.groups()
        if tag in gates:
            raise SystemExit(f"canonical DAG lists duplicate gate {tag!r}")
        gates[tag] = Gate(tag=tag, desc=desc)
    if not gates:
        raise SystemExit("canonical DAG contains no gates")
    return gates, gate_listing, canonical_json


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    dag_path = args.dag.resolve()
    profile_path = args.profile_csv.resolve()
    output_path = args.output.resolve()
    provenance_path = args.provenance_output.resolve()
    if output_path == provenance_path:
        raise SystemExit("ranking and provenance outputs must be different paths")

    steps, gate_listing, canonical_json = _load_gates(dag_path)
    rows, columns = _load_rows(profile_path, args.run_id)

    by_step: dict[str, dict[str, str]] = {}
    duplicates: list[str] = []
    for row in rows:
        tag = row["step"]
        if tag in by_step:
            duplicates.append(tag)
        by_step[tag] = row
    missing = sorted(set(steps) - set(by_step))
    extra = sorted(set(by_step) - set(steps))
    if duplicates or missing or extra:
        raise SystemExit(
            "run does not cover the canonical DAG exactly once: "
            f"duplicates={sorted(set(duplicates))}, missing={missing}, extra={extra}"
        )
    run_metadata = _validate_run_rows(rows)
    source_head = _assert_current_clean_source(run_metadata["git_sha"])

    failures = [
        row["step"]
        for row in rows
        if not _bool(row, "ok")
        or _int(row, "returncode") != 0
        or _bool(row, "timed_out")
        or _bool(row, "cpu_timed_out")
        or _int(row, "oom_kills") > 0
        or _int(row, "memory_events_oom") > 0
        or _int(row, "memory_events_oom_kill") > 0
        or _int(row, "memory_events_oom_group_kill") > 0
    ]
    if failures:
        raise SystemExit(f"run is not a clean green proof; failing/censored steps={failures}")

    ranked = sorted(rows, key=lambda row: (-_float(row, "elapsed_s"), row["step"]))
    aggregate_wall = sum(_float(row, "elapsed_s") for row in ranked)
    aggregate_cpu = sum(_float(row, "user_s") + _float(row, "sys_s") for row in ranked)
    graph_start = min(_float(row, "started_offset_s") for row in ranked)
    graph_finish = max(_float(row, "finished_offset_s") for row in ranked)

    output_rows: list[dict[str, str | int]] = []
    cumulative = 0.0
    for rank, row in enumerate(ranked, start=1):
        step = steps[row["step"]]
        wall = _float(row, "elapsed_s")
        cpu = _float(row, "user_s") + _float(row, "sys_s")
        peak = _int(row, "peak_bytes")
        cap_text = (row.get("memory_max_bytes") or "").strip()
        cap = int(cap_text) if cap_text.isdigit() else None
        memory_events_max = _int(row, "memory_events_max")
        cumulative += wall
        output_rows.append(
            {
                "rank": rank,
                "step": step.tag,
                "wall_seconds": f"{wall:.3f}",
                "aggregate_cpu_seconds": f"{cpu:.3f}",
                "peak_mib": _mib(peak),
                "cap_mib": _mib(cap) if cap is not None else cap_text or "unknown",
                "cap_used_pct": f"{100.0 * peak / cap:.1f}" if cap else "",
                "memory_events_max": memory_events_max,
                "memory_peak_censored": "true" if memory_events_max > 0 else "false",
                "aggregate_wall_share_pct": (
                    f"{100.0 * wall / aggregate_wall:.3f}" if aggregate_wall else "0.000"
                ),
                "cumulative_aggregate_wall_pct": (
                    f"{100.0 * cumulative / aggregate_wall:.3f}" if aggregate_wall else "0.000"
                ),
                "coverage_rationale": _coverage_rationale(step),
                "recommendation": _recommendation(step, wall),
            }
        )

    fieldnames = list(output_rows[0])
    ranking_buffer = io.StringIO(newline="")
    writer = csv.DictWriter(
        ranking_buffer, fieldnames=fieldnames, delimiter="\t", lineterminator="\n"
    )
    writer.writeheader()
    writer.writerows(output_rows)
    ranking_bytes = ranking_buffer.getvalue().encode()

    selected_digest = hashlib.sha256(
        json.dumps(
            [{column: row.get(column, "") for column in columns} for row in ranked],
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
    ).hexdigest()
    memory_max_event_steps = {
        row["step"]: _int(row, "memory_events_max")
        for row in ranked
        if _int(row, "memory_events_max") > 0
    }
    provenance = {
        "schema": "agent-utils-validation-gate-ranking/v2",
        "run_id": args.run_id,
        "source_git_sha": source_head,
        "source_inputs_match_clean_head_at_generation": True,
        "generated_evidence_paths_allowed_to_differ": sorted(GENERATED_EVIDENCE_PATHS),
        "run_metadata": run_metadata,
        "source_profile": str(args.profile_csv),
        "selected_profile_rows_sha256": selected_digest,
        "canonical_dag": str(args.dag),
        "canonical_root_dag_sha256": hashlib.sha256(dag_path.read_bytes()).hexdigest(),
        "canonical_expanded_dag_sha256": hashlib.sha256(canonical_json.encode()).hexdigest(),
        "canonical_gate_listing_sha256": hashlib.sha256(gate_listing.encode()).hexdigest(),
        "gate_count": len(rows),
        "all_gates_green": True,
        "memory_max_event_steps": memory_max_event_steps,
        "graph_wall_seconds": round(graph_finish - graph_start, 3),
        "aggregate_step_wall_seconds": round(aggregate_wall, 3),
        "aggregate_cpu_seconds": round(aggregate_cpu, 3),
        "ranking": str(args.output),
        "ranking_sha256": hashlib.sha256(ranking_bytes).hexdigest(),
    }
    provenance_bytes = (json.dumps(provenance, indent=2, sort_keys=True) + "\n").encode()

    # Publish the TSV first and its hash-bearing provenance last. Each replacement is atomic. A
    # process death between them leaves an old provenance file whose ranking hash does not match,
    # which is detectably incomplete rather than a plausible mixed pair.
    ranking_temporary = _stage_bytes(output_path, ranking_bytes)
    try:
        provenance_temporary = _stage_bytes(provenance_path, provenance_bytes)
    except BaseException:
        ranking_temporary.unlink(missing_ok=True)
        raise
    ranking_published = False
    provenance_published = False
    try:
        os.replace(ranking_temporary, output_path)
        ranking_published = True
        os.replace(provenance_temporary, provenance_path)
        provenance_published = True
    finally:
        if not ranking_published:
            ranking_temporary.unlink(missing_ok=True)
        if not provenance_published:
            provenance_temporary.unlink(missing_ok=True)
    print(
        f"wrote {len(rows)} green gates for run {args.run_id}: "
        f"graph_wall={graph_finish - graph_start:.3f}s, aggregate_cpu={aggregate_cpu:.3f}s, "
        f"memory_max_event_steps={len(memory_max_event_steps)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Fail-closed, per-node wall-time regression gate for portable validation.

The baseline is versioned evidence, not a constant embedded in this program.
Every selected node must produce exactly one successful profile row at the
candidate SHA.  Missing, duplicate, failed, timed-out, OOM, or regressed rows
all reject the candidate.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import re
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BASELINE = ROOT / "ci" / "validate-timing-baseline.json"
INCIDENT_FIXTURE = ROOT / "ci" / "timing-fixtures" / "92aaed5d0.json"


class InvalidEvidence(ValueError):
    """The baseline or candidate cannot support a timing judgement."""


@dataclass(frozen=True)
class Policy:
    minimum_samples: int
    percentile: float
    max_p90_seconds: float
    regression_factor: float


@dataclass(frozen=True)
class NodeBaseline:
    samples: tuple[float, ...]
    p90_seconds: float


@dataclass(frozen=True)
class Baseline:
    policy: Policy
    nodes: dict[str, NodeBaseline]


@dataclass(frozen=True)
class Candidate:
    sha: str
    elapsed_seconds: float
    returncode: int
    ok: bool
    timed_out: bool
    cpu_timed_out: bool
    oom_kills: int


def _finite_positive(value: object, label: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise InvalidEvidence(f"{label} is not numeric: {value!r}") from exc
    if not math.isfinite(number) or number <= 0:
        raise InvalidEvidence(f"{label} must be finite and positive: {number!r}")
    return number


def _boolean(value: object, label: str) -> bool:
    if value in (True, "True", "true", "1", 1):
        return True
    if value in (False, "False", "false", "0", 0):
        return False
    raise InvalidEvidence(f"{label} is not a boolean: {value!r}")


def _nearest_rank(values: tuple[float, ...], percentile: float) -> float:
    ordered = sorted(values)
    rank = max(1, math.ceil(percentile * len(ordered)))
    return ordered[rank - 1]


def load_baseline(path: Path) -> Baseline:
    try:
        raw = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise InvalidEvidence(f"cannot read baseline {path}: {exc}") from exc
    if raw.get("schema_version") != 1:
        raise InvalidEvidence("baseline schema_version must be 1")

    policy_raw = raw.get("policy", {})
    try:
        policy = Policy(
            minimum_samples=int(policy_raw["minimum_samples"]),
            percentile=float(policy_raw["percentile"]),
            max_p90_seconds=float(policy_raw["max_node_p90_seconds"]),
            regression_factor=float(policy_raw["regression_factor"]),
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise InvalidEvidence(f"baseline policy is incomplete: {exc}") from exc
    if policy.minimum_samples < 5:
        raise InvalidEvidence("baseline minimum_samples must be at least 5")
    if policy.percentile != 0.90:
        raise InvalidEvidence("baseline percentile must be exactly 0.90")
    if not math.isfinite(policy.max_p90_seconds) or policy.max_p90_seconds > 540:
        raise InvalidEvidence("baseline max_node_p90_seconds must be finite and <= 540")
    if not math.isfinite(policy.regression_factor) or policy.regression_factor <= 1:
        raise InvalidEvidence("baseline regression_factor must be finite and > 1")

    try:
        declared_node_count = int(raw["coverage"]["node_count"])
    except (KeyError, TypeError, ValueError) as exc:
        raise InvalidEvidence(f"baseline coverage is incomplete: {exc}") from exc
    if declared_node_count <= 0:
        raise InvalidEvidence("baseline coverage node_count must be positive")

    runs = raw.get("runs")
    if not isinstance(runs, list) or len(runs) < policy.minimum_samples:
        raise InvalidEvidence(
            f"baseline has {len(runs) if isinstance(runs, list) else 0} runs; "
            f"requires >= {policy.minimum_samples}"
        )
    run_shas: set[str] = set()
    for index, run in enumerate(runs):
        prefix = f"baseline run[{index}]"
        sha = run.get("sha", "")
        if not isinstance(sha, str) or re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            raise InvalidEvidence(f"{prefix} lacks an exact 40-hex SHA")
        if sha in run_shas:
            raise InvalidEvidence(f"{prefix} duplicates SHA {sha}")
        run_shas.add(sha)
        if run.get("conclusion") != "success" or run.get("coverage") != "full":
            raise InvalidEvidence(f"{prefix} is not a successful full-coverage run")
        if run.get("cold") is not True:
            raise InvalidEvidence(f"{prefix} is not recorded as a cold ephemeral-VM run")
        if int(run.get("profile_rows", -1)) != declared_node_count:
            raise InvalidEvidence(
                f"{prefix} has {run.get('profile_rows')!r} profile rows; "
                f"expected {declared_node_count}"
            )
        if int(run.get("failed_nodes", -1)) != 0:
            raise InvalidEvidence(f"{prefix} contains failed nodes")
        if int(run.get("timed_out_nodes", -1)) != 0:
            raise InvalidEvidence(f"{prefix} contains timeouts")
        if int(run.get("oom_kills", -1)) != 0:
            raise InvalidEvidence(f"{prefix} contains OOM kills")

    nodes_raw = raw.get("nodes")
    if not isinstance(nodes_raw, dict) or not nodes_raw:
        raise InvalidEvidence("baseline nodes are missing")
    nodes: dict[str, NodeBaseline] = {}
    for node, raw_samples in nodes_raw.items():
        if not isinstance(raw_samples, list) or len(raw_samples) != len(runs):
            raise InvalidEvidence(
                f"baseline node {node} has {len(raw_samples) if isinstance(raw_samples, list) else 0} "
                f"samples; expected one for each of {len(runs)} runs"
            )
        samples = tuple(
            _finite_positive(value, f"baseline {node} sample[{index}]")
            for index, value in enumerate(raw_samples)
        )
        p90 = _nearest_rank(samples, policy.percentile)
        if p90 > policy.max_p90_seconds:
            raise InvalidEvidence(
                f"baseline node {node} p90={p90:.3f}s exceeds "
                f"{policy.max_p90_seconds:.3f}s"
            )
        nodes[node] = NodeBaseline(samples=samples, p90_seconds=p90)
    if declared_node_count != len(nodes):
        raise InvalidEvidence(
            f"baseline coverage declares {declared_node_count} nodes but records {len(nodes)}"
        )
    return Baseline(policy=policy, nodes=nodes)


def load_profiles(directory: Path) -> dict[str, list[Candidate]]:
    if not directory.is_dir():
        raise InvalidEvidence(f"candidate profile directory is missing: {directory}")
    found: dict[str, list[Candidate]] = {}
    profile_files = sorted(directory.rglob("step_profiles_*.csv"))
    if not profile_files:
        raise InvalidEvidence(f"no step_profiles_*.csv under {directory}")
    for path in profile_files:
        try:
            with path.open(newline="") as handle:
                rows = list(csv.DictReader(handle))
        except (OSError, csv.Error) as exc:
            raise InvalidEvidence(f"cannot read candidate profile {path}: {exc}") from exc
        for row_index, row in enumerate(rows, start=2):
            label = f"{path}:{row_index}"
            node = row.get("step", "")
            if not node:
                raise InvalidEvidence(f"{label}: missing step")
            try:
                candidate = Candidate(
                    sha=row.get("git_sha", ""),
                    elapsed_seconds=_finite_positive(row.get("elapsed_s"), f"{label} elapsed_s"),
                    returncode=int(row.get("returncode", "")),
                    ok=_boolean(row.get("ok"), f"{label} ok"),
                    timed_out=_boolean(row.get("timed_out"), f"{label} timed_out"),
                    cpu_timed_out=_boolean(row.get("cpu_timed_out"), f"{label} cpu_timed_out"),
                    oom_kills=int(row.get("oom_kills", "")),
                )
            except (TypeError, ValueError) as exc:
                raise InvalidEvidence(f"{label}: malformed profile row: {exc}") from exc
            found.setdefault(node, []).append(candidate)
    return found


def load_fixture(path: Path) -> tuple[str, dict[str, list[Candidate]]]:
    try:
        raw = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise InvalidEvidence(f"cannot read fixture {path}: {exc}") from exc
    sha = raw.get("sha", "")
    found: dict[str, list[Candidate]] = {}
    for node, value in raw.get("nodes", {}).items():
        found[node] = [Candidate(
            sha=sha,
            elapsed_seconds=_finite_positive(value.get("elapsed_seconds"), f"fixture {node}"),
            returncode=int(value.get("returncode", 0)),
            ok=_boolean(value.get("ok", True), f"fixture {node} ok"),
            timed_out=_boolean(value.get("timed_out", False), f"fixture {node} timed_out"),
            cpu_timed_out=_boolean(value.get("cpu_timed_out", False), f"fixture {node} cpu_timed_out"),
            oom_kills=int(value.get("oom_kills", 0)),
        )]
    return sha, found


def judge(
    baseline: Baseline,
    profiles: dict[str, list[Candidate]],
    expected_sha: str,
    selected_nodes: list[str],
) -> list[str]:
    failures: list[str] = []
    if re.fullmatch(r"[0-9a-f]{40}", expected_sha) is None:
        return [f"candidate SHA must be exact 40-hex, got {expected_sha!r}"]
    if not selected_nodes:
        return ["selected-node set is empty"]
    if len(selected_nodes) != len(set(selected_nodes)):
        return ["selected-node set contains duplicates"]
    for node in selected_nodes:
        base = baseline.nodes.get(node)
        if base is None:
            failures.append(f"{node}: no recorded baseline (explicit reviewed baseline bump required)")
            continue
        rows = profiles.get(node, [])
        if len(rows) != 1:
            failures.append(f"{node}: expected exactly one timing row, found {len(rows)}")
            continue
        row = rows[0]
        if row.sha != expected_sha:
            failures.append(f"{node}: profile SHA {row.sha!r} != candidate {expected_sha}")
        if row.returncode != 0 or not row.ok:
            failures.append(f"{node}: unsuccessful timing row (returncode={row.returncode}, ok={row.ok})")
        if row.timed_out or row.cpu_timed_out:
            failures.append(
                f"{node}: timeout in timing row (wall={row.timed_out}, cpu={row.cpu_timed_out})"
            )
        if row.oom_kills != 0:
            failures.append(f"{node}: timing row records {row.oom_kills} OOM kill(s)")
        limit = min(
            base.p90_seconds * baseline.policy.regression_factor,
            baseline.policy.max_p90_seconds,
        )
        if row.elapsed_seconds > limit:
            failures.append(
                f"{node}: REGRESSION elapsed={row.elapsed_seconds:.3f}s "
                f"> limit={limit:.3f}s (baseline p90={base.p90_seconds:.3f}s x "
                f"{baseline.policy.regression_factor:g})"
            )
    return failures


def _self_test() -> None:
    policy = Policy(5, 0.90, 540.0, 1.25)
    baseline = Baseline(policy, {"test.strict_compat": NodeBaseline((60, 72, 75, 95, 101.7), 101.7)})
    sha = "9" * 40
    healthy = Candidate(sha, 110, 0, True, False, False, 0)
    incident = Candidate(sha, 730, 0, True, False, False, 0)
    assert judge(baseline, {"test.strict_compat": [healthy]}, sha, ["test.strict_compat"]) == []
    assert any("REGRESSION" in item for item in judge(
        baseline, {"test.strict_compat": [incident]}, sha, ["test.strict_compat"]
    ))
    assert any("found 0" in item for item in judge(baseline, {}, sha, ["test.strict_compat"]))
    timeout = Candidate(sha, 100, 124, False, True, False, 0)
    assert any("timeout" in item for item in judge(
        baseline, {"test.strict_compat": [timeout]}, sha, ["test.strict_compat"]
    ))
    oom = Candidate(sha, 100, 0, True, False, False, 1)
    assert any("OOM" in item for item in judge(
        baseline, {"test.strict_compat": [oom]}, sha, ["test.strict_compat"]
    ))
    assert any("no recorded baseline" in item for item in judge(
        baseline, {"new.node": [healthy]}, sha, ["new.node"]
    ))
    assert any("profile SHA" in item for item in judge(
        baseline,
        {"test.strict_compat": [Candidate("8" * 40, 100, 0, True, False, False, 0)]},
        sha,
        ["test.strict_compat"],
    ))
    assert any("exactly one" in item for item in judge(
        baseline,
        {"test.strict_compat": [healthy, healthy]},
        sha,
        ["test.strict_compat"],
    ))
    with tempfile.TemporaryDirectory() as temporary:
        missing = Path(temporary) / "missing"
        try:
            load_profiles(missing)
        except InvalidEvidence:
            pass
        else:
            raise AssertionError("missing profile directory was accepted")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", type=Path, default=DEFAULT_BASELINE)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--candidate-dir", type=Path)
    source.add_argument("--candidate-json", type=Path)
    parser.add_argument("--sha")
    parser.add_argument("--nodes", help="comma-separated exact DAG tags")
    parser.add_argument(
        "--nodes-from-baseline",
        action="store_true",
        help="require every node recorded by the baseline (local full validation)",
    )
    parser.add_argument(
        "--expect-failure",
        help="accept only if the named node is rejected (incident-replay test)",
    )
    parser.add_argument("--replay-incident", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        _self_test()
        print("validate timing gate self-test: PASS")
        return 0
    if args.replay_incident:
        args.candidate_json = INCIDENT_FIXTURE
        args.nodes = "test.strict_compat"
        args.expect_failure = "test.strict_compat"
    if not args.candidate_dir and not args.candidate_json:
        parser.error("one candidate source is required")
    if not args.nodes and not args.nodes_from_baseline:
        parser.error("--nodes or --nodes-from-baseline is required")

    try:
        baseline = load_baseline(args.baseline)
        if args.candidate_json:
            fixture_sha, profiles = load_fixture(args.candidate_json)
            expected_sha = args.sha or fixture_sha
        else:
            profiles = load_profiles(args.candidate_dir)
            expected_sha = args.sha or ""
        nodes = (
            sorted(baseline.nodes)
            if args.nodes_from_baseline
            else [node.strip() for node in args.nodes.split(",") if node.strip()]
        )
        failures = judge(baseline, profiles, expected_sha, nodes)
    except InvalidEvidence as exc:
        print(f"validate timing gate: FAIL CLOSED: {exc}", file=sys.stderr)
        return 1

    if args.expect_failure:
        matching = [
            item
            for item in failures
            if item.startswith(f"{args.expect_failure}:") and "REGRESSION" in item
        ]
        if not matching:
            print(
                f"validate timing gate: acceptance replay FAILED: {args.expect_failure} "
                "was not rejected specifically as a timing REGRESSION",
                file=sys.stderr,
            )
            return 1
        print("validate timing gate: incident replay rejected as required")
        for item in matching:
            print(f"  {item}")
        return 0
    if failures:
        print(f"validate timing gate: FAIL ({len(failures)} finding(s))", file=sys.stderr)
        for item in failures:
            print(f"  {item}", file=sys.stderr)
        return 1
    print(f"validate timing gate: PASS ({len(nodes)} node(s), exact SHA {expected_sha})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

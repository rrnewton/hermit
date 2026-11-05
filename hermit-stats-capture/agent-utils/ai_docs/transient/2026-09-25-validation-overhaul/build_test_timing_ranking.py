#!/usr/bin/env python3
"""Build the current Python per-test timing census from isolated JUnit captures.

The raw JUnit files intentionally live in ``/tmp``: they are reproducible measurement artifacts,
not repository content. This helper refuses to rank them unless their exact, phase-labelled union
equals a fresh cache-free pytest collection in both directions. The checked-in TSV and provenance
JSON are the compact durable evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import shlex
import socket
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]
PY_ROOT = REPO_ROOT / "py"
REPORT_DIR = Path(__file__).resolve().parent
DEFAULT_OUTPUT = REPORT_DIR / "test-timing-ranking.tsv"
DEFAULT_PROVENANCE_OUTPUT = REPORT_DIR / "test-timing-provenance.json"
CAPTURE_SNAPSHOT_BEFORE = Path("/tmp/agent-utils-current-capture-before.json")
CAPTURE_SNAPSHOT_AFTER = Path("/tmp/agent-utils-current-capture-after.json")
GENERATED_ARTIFACTS = frozenset(
    {
        DEFAULT_OUTPUT.relative_to(REPO_ROOT).as_posix(),
        DEFAULT_PROVENANCE_OUTPUT.relative_to(REPO_ROOT).as_posix(),
    }
)
SEPARATE_SUITE_COMPONENTS = {
    "cross-environment-hermeticity": "repository-infrastructure",
    "packaging-infrastructure": "repository-infrastructure",
    "python-cli-no-optional-deps": "repository-infrastructure",
    "python-cli-surface": "repository-infrastructure",
    "repository-dispatch-wiring": "repository-infrastructure",
    "validation-graph-contract": "repository-infrastructure",
    "wrkslots-lifecycle": "wrkslots",
}


@dataclass(frozen=True)
class PhaseInput:
    name: str
    isolation: str
    junit_paths: tuple[Path, ...]
    pytest_args: tuple[str, ...]


PHASES = (
    PhaseInput(
        "general",
        "ordinary",
        (Path("/tmp/agent-utils-current-general.xml"),),
        ("--ignore=wrkslots/tests/test_lifecycle.py",),
    ),
    PhaseInput(
        "lifecycle-namespace",
        "mapped-user-and-pid-namespace",
        tuple(
            Path(f"/tmp/agent-utils-current-lifecycle-namespace-{index}.xml")
            for index in range(6)
        ),
        ("wrkslots/tests/test_lifecycle.py", "-m", "not ordinary_environment"),
    ),
    PhaseInput(
        "lifecycle-host",
        "ordinary-host-process-namespace",
        tuple(
            Path(f"/tmp/agent-utils-current-lifecycle-host-{index}.xml")
            for index in range(3)
        ),
        ("wrkslots/tests/test_lifecycle.py", "-m", "ordinary_environment"),
    ),
)


@dataclass(frozen=True)
class Timing:
    phase: str
    isolation: str
    classname: str
    name: str
    nodeid: str
    family: str
    status: str
    seconds: float
    timing_source: str


@dataclass(frozen=True)
class JunitInput:
    phase: str
    isolation: str
    path: Path
    cases: int
    passed: int
    skipped: int
    testcase_seconds: float
    suite_seconds: float


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _sha256_file(path: Path) -> str:
    return _sha256_bytes(path.read_bytes())


def _set_digest(values: set[str]) -> str:
    return _sha256_bytes(("\n".join(sorted(values)) + "\n").encode())


def _status(testcase: ET.Element) -> str:
    if testcase.find("failure") is not None:
        return "failed"
    if testcase.find("error") is not None:
        return "error"
    if testcase.find("skipped") is not None:
        return "skipped"
    return "passed"


def _properties(testcase: ET.Element) -> dict[str, str]:
    result: dict[str, str] = {}
    for prop in testcase.findall("./properties/property"):
        name = prop.attrib.get("name")
        value = prop.attrib.get("value")
        if name is None or value is None:
            raise ValueError("JUnit property requires both name and value")
        if name in result:
            raise ValueError(f"duplicate JUnit property {name!r}")
        result[name] = value
    return result


def _read_junit(
    path: Path, phase: str, isolation: str
) -> tuple[dict[str, Timing], JunitInput]:
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise ValueError(f"cannot read JUnit input {path}: {error}") from error
    result: dict[str, Timing] = {}
    status_counts = {"passed": 0, "skipped": 0, "failed": 0, "error": 0}
    testcase_seconds = 0.0
    for testcase in root.findall(".//testcase"):
        classname = testcase.attrib["classname"]
        name = testcase.attrib["name"]
        properties = _properties(testcase)
        nodeid = properties.get("nodeid")
        family = properties.get("family")
        if nodeid is None or family is None:
            raise ValueError(
                f"{path} testcase {classname}.{name} lacks exact nodeid/family properties; "
                "capture it through scripts/run_pytest_shard.py"
            )
        expected_family = nodeid.split("[", 1)[0]
        if family != expected_family:
            raise ValueError(
                f"{path} has inconsistent family for {nodeid}: {family!r} != {expected_family!r}"
            )
        if nodeid in result:
            raise ValueError(f"duplicate nodeid in {path}: {nodeid}")
        source = PY_ROOT / nodeid.split("::", 1)[0]
        if not source.is_file():
            raise ValueError(f"{path} nodeid names no current source file: {nodeid}")
        seconds = float(testcase.attrib.get("time", "0"))
        if not math.isfinite(seconds) or seconds < 0:
            raise ValueError(f"{path} has invalid time for {nodeid}: {seconds!r}")
        status = _status(testcase)
        status_counts[status] += 1
        testcase_seconds += seconds
        result[nodeid] = Timing(
            phase=phase,
            isolation=isolation,
            classname=classname,
            name=name,
            nodeid=nodeid,
            family=family,
            status=status,
            seconds=seconds,
            timing_source="current-isolated-census",
        )
    if not result:
        raise ValueError(f"JUnit input contains no testcases: {path}")
    if status_counts["failed"] or status_counts["error"]:
        raise ValueError(
            f"JUnit input is not green: {path} "
            f"failed={status_counts['failed']} errors={status_counts['error']}"
        )
    # Pytest's xunit2 writer normally emits ``testsuites/testsuites`` with elapsed time on the
    # direct child. Accept a bare testsuite too, but never sum nested descendants twice.
    suite_nodes = [root] if root.tag == "testsuite" else root.findall("./testsuite")
    suite_times = [float(node.attrib["time"]) for node in suite_nodes if "time" in node.attrib]
    suite_seconds = sum(suite_times) if suite_times else testcase_seconds
    return result, JunitInput(
        phase=phase,
        isolation=isolation,
        path=path,
        cases=len(result),
        passed=status_counts["passed"],
        skipped=status_counts["skipped"],
        testcase_seconds=testcase_seconds,
        suite_seconds=suite_seconds,
    )


_NODEID_LINE = re.compile(r"^[^\s].*\.py::")
_ABSOLUTE_POSIX_PATH = re.compile(r"(?:^|[^\w/:])/(?!/)[^\s,;\")']+")
_ABSOLUTE_WINDOWS_PATH = re.compile(r"(?:^|[^\w/\\:])[A-Za-z]:[\\/]")
_FILE_URI = re.compile(r"(?:^|[^\w])file:(?://|/)", re.IGNORECASE)


def _collection_arguments(phase: PhaseInput) -> list[str]:
    return [
        "-m",
        "pytest",
        "--collect-only",
        "-q",
        "-p",
        "no:cacheprovider",
        "-c",
        "pyproject.toml",
        "--rootdir=.",
        *phase.pytest_args,
    ]


def _collection_command(phase: PhaseInput) -> list[str]:
    # Execute with this process's interpreter so collection and ranking cannot silently use
    # different environments. The provenance uses the portable spelling below and never records
    # this often host- or vendor-specific absolute path.
    return [sys.executable, *_collection_arguments(phase)]


def _portable_collection_command(phase: PhaseInput) -> str:
    return shlex.join(["python3", *_collection_arguments(phase)])


def _public_python_version() -> str:
    # sys.version and platform.python_version() may retain a downstream build suffix. The public
    # language version is the numeric base represented independently by sys.version_info.
    return f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"


def _collect_phase(phase: PhaseInput) -> set[str]:
    command = _collection_command(phase)
    environment = dict(os.environ)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    result = subprocess.run(
        command,
        cwd=PY_ROOT,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=180,
        check=False,
    )
    if result.returncode != 0:
        raise ValueError(
            f"live collection failed for {phase.name} (rc={result.returncode}):\n"
            f"{result.stdout}\n{result.stderr}"
        )
    nodeids = [line for line in result.stdout.splitlines() if _NODEID_LINE.match(line)]
    if not nodeids:
        raise ValueError(f"live collection produced no nodeids for {phase.name}")
    if len(nodeids) != len(set(nodeids)):
        raise ValueError(f"live collection contains duplicate nodeids for {phase.name}")
    return set(nodeids)


def _load() -> tuple[list[Timing], list[JunitInput], dict[str, set[str]]]:
    timings: dict[str, Timing] = {}
    inputs: list[JunitInput] = []
    captured_by_phase: dict[str, set[str]] = {}
    for phase in PHASES:
        phase_cases: dict[str, Timing] = {}
        for path in phase.junit_paths:
            cases, junit_input = _read_junit(path, phase.name, phase.isolation)
            overlap = set(phase_cases).intersection(cases)
            if overlap:
                raise ValueError(
                    f"nodeids occur in multiple {phase.name} shards: {sorted(overlap)[:3]}"
                )
            phase_cases.update(cases)
            inputs.append(junit_input)
        overlap = set(timings).intersection(phase_cases)
        if overlap:
            raise ValueError(f"nodeids occur in multiple phases: {sorted(overlap)[:3]}")
        live = _collect_phase(phase)
        only_capture = set(phase_cases).difference(live)
        only_collection = live.difference(phase_cases)
        if only_capture or only_collection:
            raise ValueError(
                f"{phase.name} capture/live collection mismatch: "
                f"capture_only={len(only_capture)} {sorted(only_capture)[:3]}, "
                f"collection_only={len(only_collection)} {sorted(only_collection)[:3]}"
            )
        captured_by_phase[phase.name] = set(phase_cases)
        timings.update(phase_cases)
    return (
        sorted(timings.values(), key=lambda item: (-item.seconds, item.nodeid)),
        inputs,
        captured_by_phase,
    )


def _component_mapping() -> dict[str, str]:
    manifest_path = REPO_ROOT / "validation" / "components.json"
    raw_value: object = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(raw_value, dict):
        raise ValueError("validation/components.json is not an object")
    raw: dict[str, object] = {}
    for key, value in raw_value.items():
        if not isinstance(key, str):
            raise ValueError("validation/components.json has a non-string key")
        raw[key] = value
    result: dict[str, str] = {}
    components = raw.get("components")
    separate = raw.get("separate_test_files")
    if not isinstance(components, dict) or not isinstance(separate, dict):
        raise ValueError("validation/components.json lacks component inventories")
    for component, value in components.items():
        if not isinstance(component, str) or not isinstance(value, dict):
            raise ValueError("invalid component inventory")
        test_files = value.get("test_files")
        if not isinstance(test_files, list) or not all(isinstance(item, str) for item in test_files):
            raise ValueError(f"invalid test_files for component {component}")
        for test_file in test_files:
            assert isinstance(test_file, str)
            if not test_file.startswith("py/"):
                raise ValueError(f"component test is not below py/: {test_file}")
            source = test_file.removeprefix("py/")
            previous = result.setdefault(source, component)
            if previous != component:
                raise ValueError(f"test source has multiple component owners: {source}")
    for test_file, suite in separate.items():
        if not isinstance(test_file, str) or not isinstance(suite, str):
            raise ValueError("invalid separate test inventory")
        source = test_file.removeprefix("py/")
        component = SEPARATE_SUITE_COMPONENTS.get(suite)
        if component is None:
            raise ValueError(f"unknown separate test suite {suite!r} for {test_file}")
        previous = result.setdefault(source, component)
        if previous != component:
            raise ValueError(f"test source has multiple component owners: {source}")
    return result


COMPONENT_BY_SOURCE = _component_mapping()


def _component(nodeid: str) -> str:
    source = nodeid.split("::", 1)[0]
    try:
        return COMPONENT_BY_SOURCE[source]
    except KeyError as error:
        raise ValueError(f"test source has no component owner: {source}") from error


def _coverage_contract(timing: Timing, component: str) -> str:
    lowered = timing.family.lower()
    if timing.phase.startswith("lifecycle"):
        if any(word in lowered for word in ("process", "lsof", "census", "socket", "pid")):
            return "process-liveness"
        if any(word in lowered for word in ("crash", "recover", "interrupt", "journal", "retry")):
            return "crash-recovery"
        if any(word in lowered for word in ("refus", "tamper", "invalid", "untrusted", "missing", "changed")):
            return "fail-closed-safety"
        if any(word in lowered for word in ("git", "submodule", "remote", "handoff", "archive")):
            return "git-data-preservation"
        return "lifecycle-state-machine"
    return {
        "agentctl": "agent-lifecycle-chat-state",
        "dagrun": "dag-scheduler-enforcement",
        "experiment-runner": "experiment-scheduling",
        "herdr-run": "remote-execution-session",
        "planner": "landing-plan-contract",
        "repository-infrastructure": "repository-infrastructure",
        "tick-hub": "tick-scheduling-state",
        "wrkviz": "timeline-data-contract",
        "wrkslots": "wrkslots-accounting",
    }[component]


def _recommendation(timing: Timing) -> str:
    if timing.status == "skipped":
        return "review-skip-contract"
    if timing.name == "test_process_entering_after_final_scan_before_path_move_is_not_deleted":
        return "keep-focused-proc"
    if timing.classname == "tests.test_agent_log_archive_fetcher":
        return "keep-harness-isolated"
    if timing.name.startswith("test_default_budget_blocked_traversal"):
        return "shorten-test-only-deadline"
    if timing.name.startswith("test_subject_cursor_is_fair") or timing.name.startswith(
        "test_multiple_roots_repeatedly_complete"
    ):
        return "reduce-bounded-iterations"
    if timing.name == "test_real_process_and_git_invariants":
        return "keep-isolated"
    if timing.phase == "lifecycle-namespace":
        return "keep-namespace-sharded"
    if timing.phase == "lifecycle-host":
        return "keep-host-sharded"
    if timing.seconds >= 5:
        return "keep-profile-optimize"
    return "keep-targeted"


def _escape(value: object) -> str:
    return (
        str(value)
        .replace("\\", "\\\\")
        .replace("\t", "\\t")
        .replace("\r", "\\r")
        .replace("\n", "\\n")
    )


HEADER = (
    "rank\tphase\tisolation\tcomponent\tnodeid\tfamily\tstatus\tseconds\t"
    "share_pct\tcumulative_pct\ttiming_source\tcoverage_contract\trecommendation"
)


def _rows(timings: list[Timing]) -> list[str]:
    total = sum(item.seconds for item in timings)
    cumulative = 0.0
    result: list[str] = []
    for rank, item in enumerate(timings, 1):
        cumulative += item.seconds
        component = _component(item.nodeid)
        fields = (
            rank,
            item.phase,
            item.isolation,
            component,
            item.nodeid,
            item.family,
            item.status,
            f"{item.seconds:.3f}",
            f"{100 * item.seconds / total:.8f}" if total else "0.00000000",
            f"{100 * cumulative / total:.8f}" if total else "0.00000000",
            item.timing_source,
            _coverage_contract(item, component),
            _recommendation(item),
        )
        result.append("\t".join(_escape(field) for field in fields))
    return result


def _workspace_snapshot() -> dict[str, object]:
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=REPO_ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    ).stdout.strip()
    tracked_diff = subprocess.run(
        [
            "git",
            "diff",
            "--binary",
            "HEAD",
            "--",
            ".",
            *(f":(exclude){path}" for path in sorted(GENERATED_ARTIFACTS)),
        ],
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    ).stdout
    raw_untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"],
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    ).stdout
    untracked = sorted(
        path.decode("utf-8", "surrogateescape")
        for path in raw_untracked.split(b"\0")
        if path
    )
    digest = hashlib.sha256()
    digest.update(b"git-head\0" + head.encode() + b"\0tracked-diff\0" + tracked_diff)
    bound_untracked: list[str] = []
    for relative in untracked:
        if relative in GENERATED_ARTIFACTS:
            continue
        path = REPO_ROOT / relative
        digest.update(b"untracked\0" + relative.encode("utf-8", "surrogateescape") + b"\0")
        digest.update(path.read_bytes())
        bound_untracked.append(relative)
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all", "-z"],
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
    ).stdout
    return {
        "git_head": head,
        "workspace_state": "dirty" if status else "clean",
        "workspace_snapshot_sha256": digest.hexdigest(),
        "tracked_diff_bytes": len(tracked_diff),
        "bound_untracked_file_count": len(bound_untracked),
        "generated_artifacts_excluded_from_snapshot": sorted(GENERATED_ARTIFACTS),
        "note": (
            "The digest binds HEAD, the full tracked binary diff, and every non-generated "
            "untracked file. The workspace_state field states whether that bound source snapshot "
            "was clean or dirty."
        ),
    }


def _junit_paths() -> tuple[Path, ...]:
    return tuple(path for phase in PHASES for path in phase.junit_paths)


def _junit_records() -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for path in _junit_paths():
        if not path.is_file():
            raise SystemExit(f"capture did not produce required JUnit input {path.name}")
        stat = path.stat()
        records.append(
            {
                "name": path.name,
                "bytes": stat.st_size,
                "mtime_ns": stat.st_mtime_ns,
                "sha256": _sha256_file(path),
            }
        )
    return records


def _write_snapshot(path: Path) -> None:
    snapshot = _workspace_snapshot()
    expected_names = [item.name for item in _junit_paths()]
    if path == CAPTURE_SNAPSHOT_BEFORE:
        existing = [item.name for item in _junit_paths() if item.exists()]
        if existing:
            raise SystemExit(
                "remove stale JUnit inputs before the pre-capture snapshot: "
                f"{existing}"
            )
        snapshot["junit_inputs_absent_before_capture"] = expected_names
    elif path == CAPTURE_SNAPSHOT_AFTER:
        snapshot["junit_inputs_after_capture"] = _junit_records()
    else:
        raise SystemExit(
            "--write-snapshot path must be the documented before/after capture path"
        )
    snapshot["recorded_at_utc"] = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    _atomic_write(path, (json.dumps(snapshot, indent=2, sort_keys=True) + "\n").encode())


def _verified_capture_snapshot() -> dict[str, object]:
    records: list[dict[str, object]] = []
    for path in (CAPTURE_SNAPSHOT_BEFORE, CAPTURE_SNAPSHOT_AFTER):
        try:
            raw: object = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise ValueError(f"cannot read capture snapshot {path}: {error}") from error
        if not isinstance(raw, dict):
            raise ValueError(f"capture snapshot is not an object: {path}")
        records.append(raw)
    before, after = records
    expected_names = [item.name for item in _junit_paths()]
    if before.get("junit_inputs_absent_before_capture") != expected_names:
        raise ValueError("pre-capture snapshot does not prove every JUnit input was absent")
    after_junit = after.get("junit_inputs_after_capture")
    if after_junit != _junit_records():
        raise ValueError("JUnit inputs changed after the post-capture snapshot")
    keys = ("git_head", "workspace_snapshot_sha256")
    if any(before.get(key) != after.get(key) for key in keys):
        raise ValueError(
            "source workspace changed during JUnit capture: "
            f"before={before.get('workspace_snapshot_sha256')} "
            f"after={after.get('workspace_snapshot_sha256')}"
        )
    current = _workspace_snapshot()
    if any(after.get(key) != current.get(key) for key in keys):
        raise ValueError(
            "source workspace changed after JUnit capture: "
            f"captured={after.get('workspace_snapshot_sha256')} "
            f"current={current.get('workspace_snapshot_sha256')}"
        )
    return {
        "before_path": CAPTURE_SNAPSHOT_BEFORE.name,
        "before_recorded_at_utc": before.get("recorded_at_utc"),
        "after_path": CAPTURE_SNAPSHOT_AFTER.name,
        "after_recorded_at_utc": after.get("recorded_at_utc"),
        "pre_post_match": True,
        "junit_inputs_absent_before_capture": True,
        "junit_inputs_after_capture": after_junit,
        **current,
    }


def _pareto_counts(timings: list[Timing]) -> dict[str, int]:
    total = sum(item.seconds for item in timings)
    if total == 0:
        return {f"{target}_percent": 0 for target in (50, 80, 90, 95, 99)}
    result: dict[str, int] = {}
    cumulative = 0.0
    targets = (50, 80, 90, 95, 99)
    target_index = 0
    for count, timing in enumerate(timings, 1):
        cumulative += timing.seconds
        while target_index < len(targets) and cumulative * 100 >= total * targets[target_index]:
            result[f"{targets[target_index]}_percent"] = count
            target_index += 1
    return result


def _ambient_cache_observation(live: set[str]) -> dict[str, object]:
    path = PY_ROOT / ".pytest_cache" / "v" / "cache" / "nodeids"
    display_path = path.relative_to(REPO_ROOT).as_posix()
    try:
        raw: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return {
            "path": display_path,
            "readable": False,
            "error": type(error).__name__,
        }
    if not isinstance(raw, list) or not all(isinstance(item, str) for item in raw):
        return {"path": display_path, "readable": False, "error": "not a string list"}
    cached = set(raw)
    return {
        "path": display_path,
        "readable": True,
        "entries": len(raw),
        "unique_nodeids": len(cached),
        "cache_only_stale_or_out_of_scope": len(cached - live),
        "live_only_missing_from_cache": len(live - cached),
        "authoritative": False,
    }


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_text = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary = Path(temporary_text)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(path)
    finally:
        if temporary.exists():
            temporary.unlink()


def _string_fields(value: object, path: str = "$") -> list[tuple[str, str]]:
    result: list[tuple[str, str]] = []
    if isinstance(value, str):
        result.append((path, value))
    elif isinstance(value, dict):
        for key, nested in value.items():
            result.extend(_string_fields(nested, f"{path}.{key}"))
    elif isinstance(value, list):
        for index, nested in enumerate(value):
            result.extend(_string_fields(nested, f"{path}[{index}]"))
    return result


def _assert_public_provenance(provenance: dict[str, object]) -> None:
    environment = provenance.get("environment")
    if not isinstance(environment, dict):
        raise ValueError("provenance environment must be an object")
    python_version = environment.get("python")
    if (
        not isinstance(python_version, str)
        or re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", python_version) is None
    ):
        raise ValueError(
            "provenance Python version must be a public numeric base version"
        )

    collection = provenance.get("collection_contract")
    phases = collection.get("phases") if isinstance(collection, dict) else None
    if not isinstance(phases, list):
        raise ValueError("provenance collection phases must be a list")
    for index, phase in enumerate(phases):
        command = (
            phase.get("live_collection_command") if isinstance(phase, dict) else None
        )
        if not isinstance(command, str) or shlex.split(command)[:3] != [
            "python3",
            "-m",
            "pytest",
        ]:
            raise ValueError(
                f"provenance collection phase {index} must use portable python3 -m pytest"
            )

    hostname = socket.gethostname()
    runtime_identities = {
        str(REPO_ROOT),
        str(PY_ROOT),
        str(Path.home()),
        sys.executable,
        sys.prefix,
        sys.exec_prefix,
        sys.base_prefix,
        hostname,
    }
    if "." in hostname:
        runtime_identities.add(hostname.split(".", 1)[0])
    runtime_identities.update(
        str(Path(value).resolve())
        for value in tuple(runtime_identities)
        if value.startswith("/")
    )
    runtime_identities.difference_update({"", "/", "."})
    for field, value in _string_fields(provenance):
        if (
            _ABSOLUTE_POSIX_PATH.search(value)
            or _ABSOLUTE_WINDOWS_PATH.search(value)
            or _FILE_URI.search(value)
        ):
            raise ValueError(f"provenance field {field} contains an absolute path")
        if any(identity in value for identity in runtime_identities):
            raise ValueError(
                f"provenance field {field} contains a runtime host identity"
            )


def _privacy_self_test() -> int:
    commands = [_portable_collection_command(phase) for phase in PHASES]
    for phase, command in zip(PHASES, commands, strict=True):
        if shlex.split(command) != ["python3", *_collection_arguments(phase)]:
            raise AssertionError("portable collection command changed meaning")
        if _collection_command(phase)[0] != sys.executable:
            raise AssertionError(
                "live collection no longer uses the running interpreter"
            )

    provenance: dict[str, object] = {
        "environment": {"python": _public_python_version()},
        "collection_contract": {
            "phases": [{"live_collection_command": command} for command in commands]
        },
        "source_snapshot": {
            "before_path": CAPTURE_SNAPSHOT_BEFORE.name,
            "after_path": CAPTURE_SNAPSHOT_AFTER.name,
        },
        "public_reference": "https://example.com/a/b",
    }
    _assert_public_provenance(provenance)

    def expect_rejection(field: str, value: str) -> None:
        candidate = json.loads(json.dumps(provenance))
        if field == "python":
            candidate["environment"]["python"] = value
        else:
            candidate[field] = value
        try:
            _assert_public_provenance(candidate)
        except ValueError:
            return
        raise AssertionError(f"privacy check accepted the {field} regression fixture")

    expect_rejection("python", f"{_public_python_version()}+vendor")
    expect_rejection("absolute_path", "/private/toolchain/python3")
    expect_rejection("bracketed_path", "[/private/toolchain/python3]")
    expect_rejection("backtick_path", "`/private/toolchain/python3`")
    expect_rejection("braced_path", "{/private/toolchain/python3}")
    expect_rejection("windows_path", r"{C:\private\toolchain\python.exe}")
    expect_rejection("file_uri", "file:///private/toolchain/python3")
    hostname = socket.gethostname()
    if hostname:
        expect_rejection("hostname", hostname)
    print("build_test_timing_ranking --self-test: PASSED")
    return 0


def _provenance(
    timings: list[Timing],
    inputs: list[JunitInput],
    captured_by_phase: dict[str, set[str]],
    ranking_path: Path,
    capture_snapshot: dict[str, object],
) -> dict[str, object]:
    ranking_bytes = ranking_path.read_bytes()
    phase_records: list[dict[str, object]] = []
    for phase in PHASES:
        phase_timings = [item for item in timings if item.phase == phase.name]
        phase_inputs = [item for item in inputs if item.phase == phase.name]
        phase_records.append(
            {
                "name": phase.name,
                "isolation": phase.isolation,
                "cases": len(phase_timings),
                "passed": sum(item.status == "passed" for item in phase_timings),
                "skipped": sum(item.status == "skipped" for item in phase_timings),
                "testcase_seconds": round(sum(item.seconds for item in phase_timings), 3),
                "sum_shard_suite_seconds": round(
                    sum(item.suite_seconds for item in phase_inputs), 3
                ),
                "nodeid_set_sha256": _set_digest(captured_by_phase[phase.name]),
                "live_collection_command": _portable_collection_command(phase),
                "exact_capture_collection_equality": True,
            }
        )
    source_artifacts = [
        {
            "phase": item.phase,
            "isolation": item.isolation,
            "path": item.path.name,
            "bytes": item.path.stat().st_size,
            "sha256": _sha256_file(item.path),
            "cases": item.cases,
            "passed": item.passed,
            "skipped": item.skipped,
            "testcase_seconds": round(item.testcase_seconds, 3),
            "suite_seconds": round(item.suite_seconds, 3),
        }
        for item in inputs
    ]
    union = {item.nodeid for item in timings}
    provenance: dict[str, object] = {
        "schema": "agent-utils-validation-test-timing-provenance/v2",
        "generated_at_utc": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "privacy": {
            "host_identifier": "redacted-shared-development-host",
            "internal_paths_and_vendor_runtime_suffixes_omitted": True,
        },
        "source_snapshot": capture_snapshot,
        "environment": {
            "python": _public_python_version(),
            "logical_cpus": os.cpu_count(),
            "note": "One shared-host census; use as a scheduling seed, not a stable benchmark.",
        },
        "collection_contract": {
            "cache_provider_disabled": True,
            "reason": (
                "pytest's cache/nodeids is an accumulating historical union and contained stale "
                "entries; the generator performs a fresh live collection for every phase."
            ),
            "phase_sets_pairwise_disjoint": True,
            "phase_union_matches_capture_exactly": True,
            "testcase_count": len(timings),
            "unique_nodeid_count": len(union),
            "union_nodeid_set_sha256": _set_digest(union),
            "ambient_cache_observation": _ambient_cache_observation(union),
            "phases": phase_records,
        },
        "current_census": {
            "result": "green",
            "testcase_count": len(timings),
            "passed": sum(item.status == "passed" for item in timings),
            "skipped": sum(item.status == "skipped" for item in timings),
            "sum_testcase_seconds": round(sum(item.seconds for item in timings), 3),
            "pareto_case_counts": _pareto_counts(timings),
        },
        "ranking_artifact": {
            "path": ranking_path.relative_to(REPO_ROOT).as_posix(),
            "bytes": len(ranking_bytes),
            "lines_including_header": len(timings) + 1,
            "data_rows": len(timings),
            "columns": len(HEADER.split("\t")),
            "sha256": _sha256_bytes(ranking_bytes),
            "sort": "seconds descending, then nodeid ascending",
            "cumulative_percent_last_row": 100.0,
        },
        "generator_artifact": {
            "path": Path(__file__).resolve().relative_to(REPO_ROOT).as_posix(),
            "bytes": Path(__file__).stat().st_size,
            "sha256": _sha256_file(Path(__file__)),
        },
        "source_artifacts": source_artifacts,
        "identity_contract": {
            "source": "pytest xunit2 testcase properties written by run_pytest_shard.py",
            "required_properties": ["family", "nodeid"],
            "legacy_classname_reconstruction_used": False,
            "capture_collection_misses": 0,
            "collection_capture_misses": 0,
            "collisions": 0,
        },
        "coverage_contract_codes": {
            "agent-lifecycle-chat-state": "Agent lifecycle, chat transport, durable state, and command safety.",
            "crash-recovery": "Crash boundary, replay, recovery, and idempotence behavior.",
            "dag-scheduler-enforcement": "DAG parsing, scheduling, resource enforcement, attribution, and CLI behavior.",
            "experiment-scheduling": "Experiment planning, calibration, limits, and reporting.",
            "fail-closed-safety": "Malformed, changed, missing, or untrusted evidence must refuse safely.",
            "git-data-preservation": "Git/worktree/submodule/handoff state must not be lost during lifecycle mutation.",
            "landing-plan-contract": "Landing graph, prioritization, collection, and CLI behavior.",
            "lifecycle-state-machine": "Wrkslots state transitions and durable lifecycle invariants.",
            "process-liveness": "Real process identity, namespace, descriptor, socket, cgroup, and lsof evidence.",
            "repository-infrastructure": "Repository launchers, package/build wiring, selectors, and hermeticity.",
            "remote-execution-session": "Remote runner configuration, allowlisting, session state, and readiness.",
            "tick-scheduling-state": "Tick cadence, probe, persistence, and CLI behavior.",
            "timeline-data-contract": "Timeline ingestion, normalization, query, storage, rendering, and browser behavior.",
            "wrkslots-accounting": "Wrkslots census, budget, reservation, and cache-accounting behavior.",
        },
        "recommendation_codes": {
            "keep-focused-proc": "Keep the real race while enumerating only its owned fixture PID.",
            "keep-harness-isolated": "Keep behavior while suppressing unrelated ambient-agent delay.",
            "shorten-test-only-deadline": "Keep timeout/kill/reap behavior with a short explicit test budget.",
            "reduce-bounded-iterations": "Keep fairness observations but stop when declared evidence is satisfied.",
            "keep-isolated": "Retain high-value real integration behavior as an isolated DAG node.",
            "keep-namespace-sharded": "Retain destructive coverage in independent user/PID namespaces.",
            "keep-host-sharded": "Retain host-identity behavior with contamination-aware sharding.",
            "keep-profile-optimize": "Retain the scenario and optimize its setup or waits.",
            "keep-targeted": "Cost is modest; select through component ownership.",
            "review-skip-contract": "Confirm the skip is an intentional platform contract, not absent coverage.",
        },
    }
    _assert_public_provenance(provenance)
    return provenance


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--start", type=int, default=0)
    parser.add_argument("--end", type=int)
    parser.add_argument("--header", action="store_true")
    parser.add_argument("--verify", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--provenance-output", type=Path)
    parser.add_argument("--write-snapshot", type=Path)
    args = parser.parse_args()
    if args.self_test:
        if any(
            value
            for value in (
                args.verify,
                args.output is not None,
                args.provenance_output is not None,
                args.write_snapshot is not None,
                args.header,
                args.start != 0,
                args.end is not None,
            )
        ):
            parser.error("--self-test cannot be combined with other options")
        return _privacy_self_test()
    if args.write_snapshot is not None:
        if any(
            value
            for value in (
                args.verify,
                args.output is not None,
                args.provenance_output is not None,
                args.header,
                args.start != 0,
                args.end is not None,
            )
        ):
            parser.error("--write-snapshot cannot be combined with ranking options")
        _write_snapshot(args.write_snapshot.resolve())
        return 0
    if args.start < 0 or (args.end is not None and args.end < args.start):
        parser.error("invalid row range")
    if args.provenance_output is not None and args.output is None:
        parser.error("--provenance-output requires --output")

    capture_snapshot = _verified_capture_snapshot()
    timings, inputs, captured_by_phase = _load()
    rows = _rows(timings)
    if args.verify:
        print(
            json.dumps(
                {
                    "rows": len(rows),
                    "unique_nodeids": len({item.nodeid for item in timings}),
                    "seconds": round(sum(item.seconds for item in timings), 3),
                    "phase_counts": {
                        phase.name: len(captured_by_phase[phase.name]) for phase in PHASES
                    },
                    "nodeid_set_sha256": _set_digest({item.nodeid for item in timings}),
                    "first": timings[0].nodeid,
                    "last": timings[-1].nodeid,
                },
                sort_keys=True,
            )
        )
        return 0

    selected_rows = rows[args.start : args.end]
    prefix = [HEADER] if args.header or args.output is not None else []
    rendered = "\n".join([*prefix, *selected_rows]) + "\n"
    if args.output is None:
        print(rendered, end="")
        return 0
    if args.start != 0 or args.end is not None:
        parser.error("--output cannot be combined with a row range")
    output = args.output.resolve()
    _atomic_write(output, rendered.encode())
    if args.provenance_output is not None:
        provenance = _provenance(
            timings, inputs, captured_by_phase, output, capture_snapshot
        )
        _atomic_write(
            args.provenance_output.resolve(),
            (json.dumps(provenance, indent=2, sort_keys=True) + "\n").encode(),
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

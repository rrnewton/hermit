"""Nested ``agentctl-session/v2`` and ``v3`` registry rows, compared across editions.

Each case starts one worker per edition and then replaces its registry row with the same
nested fixture; only the case root differs. Both editions must then decode it, report it,
write it back in its nested form, refuse the same malformed rows with the same words, and
let ``anchor`` migrate it to flat schema 1 with the same ``migrated_from``.
"""
from __future__ import annotations

import copy
import json
import shutil
from collections.abc import Callable
from pathlib import Path
from typing import cast

from agentctl_differential import (
    _COMMON, _change, _json, _normalize, _pair, _retire_fixture_processes, _start,
    _submission_count,
)
from herdr_agent_differential import Harness, PairCase, Report

_V2 = "agentctl-session/v2"
_V3 = "agentctl-session/v3"
_EXTENSIONS: dict[str, object] = {"review": {"labels": ["persistent"]}}
_V2_ONLY = ("session_agent", "session_value", "goal_delivery", "goal_session_id",
            "goal_messages", "goal_message_id", "goal_command")


def _native(value: str, source: str, agent: str = "codex") -> dict[str, object]:
    return {"schema": "agentctl-native-session/v1", "agent": agent, "value": value, "source": source}


def _fixture(root: Path, schema: str, *, session_value: str | None = "session-1",
             goal_session_id: str | None = None,
             native_session: dict[str, object] | None = None) -> dict[str, object]:
    """The canonical nested row for the started worker of one case root."""
    row: dict[str, object] = {
        "schema": _V2, "name": "worker", "token": "nested-generation", "created_at": 1.5,
        "lifecycle": "running",
        "launch": {
            "schema": "agentctl-launch/v1", "harness": "codex", "cwd": str(root),
            "adapter": "herdr", "mode": "interactive", "backend": "herdr", "model": None,
            "resume": None, "profile": None, "argv": ["codex", "--no-alt-screen"],
            "environment_names": [], "runtime_home": None, "runtime_ownership": "owned",
            "executable": None,
        },
        "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": "w1:p1",
        "session_agent": None if session_value is None else "codex",
        "session_value": session_value,
        "startup_warning": None, "effective_reasoning_effort": None, "error": None,
        "goal": None, "goal_delivery": None, "goal_session_id": goal_session_id,
        "goal_command": None, "goal_messages": {}, "goal_message_id": None,
        "paused": False, "pane_reported_by_agentctl": False,
        "custom_process_identity": None, "foreign_shell_identity": None,
        "runner_identity": None, "extensions": copy.deepcopy(_EXTENSIONS),
    }
    if schema == _V3:
        for key in _V2_ONLY:
            row.pop(key)
        row["schema"] = _V3
        row["native_session"] = native_session
        row["goal"] = {"schema": "agentctl-goal/v1", "objective": None,
                       "message_id": None, "native_command": None}
    return row


def _record_path(root: Path) -> Path:
    return root / "registry" / "worker" / "agent.json"


def _write(case: PairCase, build: Callable[[Path], dict[str, object]]) -> None:
    for root in (case.python_root, case.rust_root):
        _record_path(root).write_text(json.dumps(build(root)), encoding="utf-8")


def _stored(root: Path, path: Path | None = None) -> object:
    """One edition's row with its case root, token and timestamps normalized."""
    text = (path or _record_path(root)).read_text(encoding="utf-8")
    return _normalize(json.loads(text.replace(str(root), "<ROOT>")))


def _row(value: object) -> dict[str, object]:
    return cast(dict[str, object], value) if isinstance(value, dict) else {}


def _same_rows(report: Report, case: PairCase, label: str,
               check: Callable[[dict[str, object]], bool]) -> dict[str, object]:
    python, rust = _stored(case.python_root), _stored(case.rust_root)
    report.require(label, python == rust and check(_row(python)),
                   f"stored rows differ or are wrong: python={python!r} rust={rust!r}")
    return _row(python)


def _started(harness: Harness, report: Report, label: str,
             state: dict[str, object] | None = None) -> PairCase | None:
    case = harness.case(label, state)
    return case if _start(harness, report, case, label + "/start") else None


def _v2_observed(harness: Harness, report: Report) -> None:
    """Read, report, write back, send, refuse rename, then anchor-migrate a v2 row."""
    label = "nested/v2-observed"
    case = _started(harness, report, label)
    if case is None:
        return
    _write(case, lambda root: _fixture(root, _V2))
    python, _ = _pair(harness, report, case, label + "/status", ("status", "worker", *_COMMON))
    status = _row(_json(python))
    report.require(label + "/status-nested", status.get("schema") == _V2
                   and "harness" not in status and isinstance(status.get("launch"), dict)
                   and "anchor" in cast(list[object], status.get("capabilities", [])),
                   f"status lost the nested form or its capabilities: {status!r}")
    _pair(harness, report, case, label + "/list", ("list", *_COMMON))
    _pair(harness, report, case, label + "/pause", ("pause", "worker", *_COMMON))
    _same_rows(report, case, label + "/pause-written-back", lambda row: (
        row.get("schema") == _V2 and row.get("paused") is True
        and row.get("extensions") == _EXTENSIONS
        and not {"harness", "cwd", "adapter", "mode", "backend", "arguments"} & set(row)))
    _pair(harness, report, case, label + "/resume", ("resume", "worker", *_COMMON))
    _pair(harness, report, case, label + "/send", ("send", "worker", "nested hello", *_COMMON))
    report.require(label + "/send-delivered", all(
        _submission_count(root, "nested hello") == 1 for root in (case.python_root, case.rust_root)),
        "send to a nested row was not delivered exactly once")
    outcomes = harness.invoke(case, ("rename", "worker", "reviewer", *_COMMON))
    report.require(label + "/rename-refused", all(
        outcome.returncode == 75 and "uses a nested record format" in outcome.stderr
        and "agentctl anchor worker" in outcome.stderr for outcome in outcomes)
        and _same_rows(report, case, label + "/rename-unchanged",
                       lambda row: row.get("schema") == _V2).get("name") == "worker",
        f"rename did not refuse the nested row alike: {outcomes!r}")
    python, _ = _pair(harness, report, case, label + "/anchor", ("anchor", "worker", *_COMMON))
    report.require(label + "/anchor-migrated-from",
                   _row(_json(python)).get("migrated_from") == _V2,
                   f"anchor did not report the migration: {python!r}")
    _same_rows(report, case, label + "/anchor-flat", lambda row: (
        row.get("schema") == 1 and row.get("harness") == "codex"
        and row.get("arguments") == ["--no-alt-screen"] and row.get("session_value") == "session-1"
        and row.get("anchor_rule") == 2 and row.get("terminal_id") == "term-1"
        and "runner_identity" in row and not {"launch", "extensions"} & set(row)))
    python, _ = _pair(harness, report, case, label + "/anchor-again", ("anchor", "worker", *_COMMON))
    report.require(label + "/anchor-again-flat", "migrated_from" in _row(_json(python))
                   and _row(_json(python))["migrated_from"] is None,
                   f"a flat row was reported as migrated: {python!r}")
    _retire_fixture_processes(case)


def _v3_asserted(harness: Harness, report: Report) -> None:
    """An asserted v3 session never pins routing or anchors input, and stays asserted."""
    label = "nested/v3-asserted"
    case = _started(harness, report, label)
    if case is None:
        return
    _write(case, lambda root: _fixture(
        root, _V3, native_session=_native("asserted-session", "asserted")))
    # The pane reports no session, as Herdr may; an asserted session must not be required.
    _change(case, {"sessionless": True})
    _pair(harness, report, case, label + "/status", ("status", "worker", *_COMMON))
    outcomes = harness.invoke(case, ("doctor", *_COMMON))
    findings = [_json(outcome) for outcome in outcomes]
    report.require(label + "/doctor-unanchored",
                   outcomes[0].returncode == outcomes[1].returncode == 1
                   and findings[0] == findings[1] and "unanchored" in json.dumps(findings[0]),
                   f"doctor disagreed about an asserted session: {outcomes!r}")
    outcomes = harness.invoke(case, ("send", "worker", "before anchor", *_COMMON))
    report.require(label + "/send-unanchored",
                   outcomes[0].returncode == outcomes[1].returncode == 75
                   and _json(outcomes[0]) == _json(outcomes[1])
                   and all(_submission_count(root, "before anchor") == 0
                           for root in (case.python_root, case.rust_root)),
                   f"an asserted session authorized input: {outcomes!r}")
    python, _ = _pair(harness, report, case, label + "/anchor", ("anchor", "worker", *_COMMON))
    report.require(label + "/anchor-migrated-from",
                   _row(_json(python)).get("migrated_from") == _V3,
                   f"anchor did not report the migration: {python!r}")
    _same_rows(report, case, label + "/anchor-flat", lambda row: (
        row.get("schema") == 1 and row.get("goal_session_id") == "asserted-session"
        and row.get("session_agent") is None and row.get("session_value") is None
        and row.get("native_session") == _native("asserted-session", "asserted")))
    _pair(harness, report, case, label + "/send-anchored", ("send", "worker", "after anchor", *_COMMON))
    _retire_fixture_processes(case)


def _launch(row: dict[str, object]) -> dict[str, object]:
    return cast(dict[str, object], row["launch"])


#: Rows each refused while decoding the nested form, with the words both editions use.
_REFUSALS: tuple[tuple[str, str, Callable[[dict[str, object]], object]], ...] = (
    ("flat-duplicate", _V2, lambda row: row.update(harness="claude")),
    ("missing-field", _V2, lambda row: row.pop("extensions")),
    ("launch-not-object", _V2, lambda row: row.update(launch=[])),
    ("launch-schema", _V2, lambda row: _launch(row).update(schema="agentctl-launch/v9")),
    ("launch-extra-field", _V2, lambda row: _launch(row).update(extra=None)),
    ("v1-permission-mode", _V2, lambda row: _launch(row).update(permission_mode=None)),
    ("v2-permission-mode", _V2, lambda row: _launch(row).update(
        schema="agentctl-launch/v2", permission_mode="bypass")),
    ("lifecycle", _V2, lambda row: row.update(lifecycle="banana")),
    ("profile", _V2, lambda row: _launch(row).update(profile=17)),
    ("runtime-home", _V2, lambda row: _launch(row).update(runtime_home="/srv/runtime")),
    ("headless", _V2, lambda row: _launch(row).update(mode="headless")),
    ("relative-cwd", _V2, lambda row: _launch(row).update(cwd="project")),
    ("foreign-ownership", _V2, lambda row: _launch(row).update(runtime_ownership="foreign")),
    ("pane-adapter-codex", _V2, lambda row: _launch(row).update(adapter="herdr-pane")),
    ("empty-model", _V2, lambda row: _launch(row).update(model="")),
    ("empty-argument", _V2, lambda row: _launch(row).update(argv=["codex", ""])),
    ("environment-name", _V2, lambda row: _launch(row).update(environment_names=["1BAD"])),
    ("executable", _V2, lambda row: _launch(row).update(
        executable={"path": "relative", "device": 1, "inode": 1})),
    ("argv-program", _V2, lambda row: _launch(row).update(argv=["claude", "--no-alt-screen"])),
    ("argv-model", _V2, lambda row: _launch(row).update(model="gpt-test")),
    ("presets", _V2, lambda row: _launch(row).update(
        harness="aider", model="gpt-test", argv=["aider", "--no-alt-screen"])),
    ("runner-identity", _V2, lambda row: row.update(runner_identity={"pid": 1})),
    ("relay-identity", _V2, lambda row: _launch(row).update(adapter="herdr-relay")),
    ("extensions", _V2, lambda row: row.update(extensions={"harness": 1})),
    ("session-agent", _V2, lambda row: row.update(session_agent="claude")),
    ("contradictory-sessions", _V2, lambda row: row.update(goal_session_id="other-session")),
    ("v3-native-agent", _V3, lambda row: row.update(native_session=_native("session-1", "observed", "claude"))),
    ("v3-native-source", _V3, lambda row: row.update(native_session=_native("session-1", "guessed"))),
    ("v3-goal", _V3, lambda row: cast(dict[str, object], row["goal"]).pop("native_command")),
)


def _write_refusals(root: Path) -> list[Path]:
    """One private registry row per malformed fixture, each beside the healthy worker."""
    directories: list[Path] = []
    for name, schema, mutation in _REFUSALS:
        directory = root / "registry" / f"bad-{name}"
        directory.mkdir(mode=0o700)
        directory.chmod(0o700)
        row = _fixture(root, schema, native_session=_native("session-1", "observed"))
        row["name"] = directory.name
        mutation(row)
        record = directory / "agent.json"
        record.write_text(json.dumps(row), encoding="utf-8")
        record.chmod(0o600)
        directories.append(directory)
    return directories


def _rows(harness: Harness, report: Report) -> None:
    """Asserted v2 migration, v3 bind-session, malformed rows, and stop, on one worker."""
    label = "nested/rows"
    case = _started(harness, report, label)
    if case is None:
        return
    # A v2 row with only an asserted goal session reads, reports and migrates alike.
    _write(case, lambda root: _fixture(root, _V2, session_value=None,
                                       goal_session_id="asserted-session"))
    _change(case, {"sessionless": True})
    _pair(harness, report, case, label + "/v2-asserted/status", ("status", "worker", *_COMMON))
    python, _ = _pair(harness, report, case, label + "/v2-asserted/anchor",
                      ("anchor", "worker", *_COMMON))
    report.require(label + "/v2-asserted/anchor-migrated-from",
                   _row(_json(python)).get("migrated_from") == _V2,
                   f"anchor did not report the migration: {python!r}")
    _same_rows(report, case, label + "/v2-asserted/anchor-flat", lambda row: (
        row.get("schema") == 1 and row.get("goal_session_id") == "asserted-session"
        and row.get("session_value") is None))
    # bind-session on a v3 row stores one asserted native session in the nested form.
    _write(case, lambda root: _fixture(root, _V3, native_session=None))
    _pair(harness, report, case, label + "/v3-bind",
          ("bind-session", "worker", "manual-session", *_COMMON))
    _same_rows(report, case, label + "/v3-bind/written-back", lambda row: (
        row.get("schema") == _V3
        and row.get("native_session") == _native("manual-session", "asserted")))
    _pair(harness, report, case, label + "/v3-bind/status", ("status", "worker", *_COMMON))
    # Every malformed row is its own record_error row with the same message in both editions.
    written = [_write_refusals(root) for root in (case.python_root, case.rust_root)]
    python, _ = _pair(harness, report, case, label + "/refusals", ("list", *_COMMON), expected=1)
    rows = _json(python)
    errors = {str(_row(row).get("name")): _row(row) for row in rows} if isinstance(rows, list) else {}
    for name, _schema, _mutation in _REFUSALS:
        row = errors.get(f"bad-{name}", {})
        report.require(f"{label}/refusals/{name}", row.get("record_error") is True
                       and f"<ROOT>/registry/bad-{name}/agent.json" in str(row.get("probe_error")),
                       f"malformed nested row was not a record error: {row!r}")
    for directories in written:
        for directory in directories:
            shutil.rmtree(directory)
    # Stop archives a nested row in its nested form.
    _write(case, lambda root: _fixture(root, _V3, native_session=_native("session-1", "observed")))
    _change(case, {"sessionless": False})
    _pair(harness, report, case, label + "/stop", ("stop", "worker", *_COMMON))
    archives = [sorted((root / "registry" / "archive").glob("*/agent.json"))
                for root in (case.python_root, case.rust_root)]
    stopped = [_stored(root, paths[0]) if len(paths) == 1 else None
               for root, paths in zip((case.python_root, case.rust_root), archives)]
    report.require(label + "/stop/archived-nested", stopped[0] == stopped[1]
                   and _row(stopped[0]).get("schema") == _V3
                   and _row(stopped[0]).get("lifecycle") == "stopped",
                   f"stop did not archive the nested row alike: {stopped!r}")
    _retire_fixture_processes(case)


def nested_records(harness: Harness, report: Report) -> None:
    """Both editions read, write back and anchor the same v2/v3 rows identically."""
    _v2_observed(harness, report)
    _v3_asserted(harness, report)
    _rows(harness, report)

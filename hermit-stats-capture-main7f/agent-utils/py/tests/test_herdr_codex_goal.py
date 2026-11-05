"""Native goal operations exercise a real, isolated JSON-RPC subprocess."""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

import pytest

import agentctl.codex_goal as goal_module
from agentctl.codex_goal import CodexGoalError, _main, clear_goal, get_goal, set_goal
from agentctl.jsonx import as_mapping

_SERVER = r'''
import json, pathlib, subprocess, sys, time
mode, directory = sys.argv[1:]
root = pathlib.Path(directory)
def reply(value):
    text = json.dumps(value) + '\n'
    if mode == 'fragmented':
        sys.stdout.write(text[:7]); sys.stdout.flush(); time.sleep(0.01)
        text = text[7:]
    sys.stdout.write(text); sys.stdout.flush()
for line in sys.stdin:
    request = json.loads(line)
    with (root / 'calls.jsonl').open('a') as log:
        log.write(json.dumps(request) + '\n')
    method = request['method']
    if method == 'initialized':
        continue
    if method == 'initialize':
        reply({'id': request['id'], 'result': {'userAgent': 'fixture'}})
        continue
    if mode == 'hang':
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
        (root / 'child.pid').write_text(str(child.pid))
        time.sleep(60)
    if mode == 'error':
        reply({'id': request['id'], 'error': {'code': -32601, 'message': 'unknown goal method'}})
        continue
    if mode == 'invalid':
        print('invalid-json', flush=True)
        continue
    if mode == 'interactive':
        reply({'id': 98, 'method': 'item/requestApproval', 'params': {}})
        continue
    if mode == 'stderr':
        sys.stderr.write('launcher-log\n' * 20000); sys.stderr.flush()
    if mode == 'closed':
        print('fixture transport failed', file=sys.stderr, flush=True)
        sys.exit(4)
    reply({'method': 'thread/goal/updated', 'params': {}})
    if method == 'thread/goal/clear':
        result = {}
    elif mode == 'absent':
        result = {'goal': None}
    else:
        params = request['params']
        goal = {'threadId': params['threadId'], 'objective': params.get('objective', 'Observe progress'),
                'status': params.get('status', 'active'), 'tokenBudget': params.get('tokenBudget'),
                'tokensUsed': 10, 'timeUsedSeconds': 2, 'createdAt': 100, 'updatedAt': 101}
        if mode == 'wrong-session':
            goal['threadId'] = 'unrelated-session'
        if mode == 'bad-counter':
            goal['tokensUsed'] = True
        result = {'goal': goal}
    reply({'id': 99 if mode == 'wrong-id' else request['id'], 'result': result})
'''


def _server(tmp_path: Path, mode: str = "normal") -> list[str]:
    return [sys.executable, "-u", "-c", _SERVER, mode, str(tmp_path)]


def _calls(tmp_path: Path) -> list[dict[str, object]]:
    result: list[dict[str, object]] = []
    for line in (tmp_path / "calls.jsonl").read_text().splitlines():
        value: object = json.loads(line)
        result.append(as_mapping(value, "recorded request"))
    return result


@pytest.mark.parametrize("mode", ["normal", "fragmented", "stderr"])
def test_read_has_no_resume_or_turn_side_effects(tmp_path: Path, mode: str) -> None:
    result = get_goal("thread-fixture", _server(tmp_path, mode))
    assert result is not None
    assert result["objective"] == "Observe progress"
    assert result["tokensUsed"] == 10
    calls = _calls(tmp_path)
    assert [call["method"] for call in calls] == [
        "initialize", "initialized", "thread/goal/get",
    ]
    assert calls[-1]["params"] == {"threadId": "thread-fixture"}


def test_default_connects_to_existing_configured_transport(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(goal_module, "DEFAULT_COMMAND", _server(tmp_path))
    assert get_goal("thread-fixture") is not None


def test_absent_goal_is_distinct_from_protocol_failure(tmp_path: Path) -> None:
    assert get_goal("thread-fixture", _server(tmp_path, "absent")) is None


def test_set_omits_unspecified_fields_and_preserves_literal_text(tmp_path: Path) -> None:
    objective = "Inspect $(never-execute) and `literal`\nThen report π."
    result = set_goal("thread-fixture", objective, command=_server(tmp_path))
    assert result["objective"] == objective
    assert _calls(tmp_path)[-1] == {
        "id": 2,
        "method": "thread/goal/set",
        "params": {"threadId": "thread-fixture", "objective": objective},
    }


def test_set_status_and_budget_does_not_replace_objective(tmp_path: Path) -> None:
    result = set_goal(
        "thread-fixture", status="paused", token_budget=100,
        command=_server(tmp_path),
    )
    assert result["status"] == "paused"
    assert result["tokenBudget"] == 100
    assert _calls(tmp_path)[-1]["params"] == {
        "threadId": "thread-fixture", "status": "paused", "tokenBudget": 100,
    }


def test_clear_is_a_single_goal_operation(tmp_path: Path) -> None:
    clear_goal("thread-fixture", _server(tmp_path))
    assert _calls(tmp_path)[-1]["method"] == "thread/goal/clear"


@pytest.mark.parametrize(
    ("mode", "message"),
    [
        ("error", "unknown goal method"),
        ("invalid", "invalid Codex goal response"),
        ("interactive", "no approval was sent"),
        ("closed", "transport closed"),
        ("wrong-session", "belongs to another session"),
        ("bad-counter", "not an integer"),
        ("wrong-id", "unexpected response ID"),
    ],
)
def test_protocol_failures_are_actionable(
    tmp_path: Path, mode: str, message: str,
) -> None:
    with pytest.raises(CodexGoalError, match=message):
        get_goal("thread-fixture", _server(tmp_path, mode))


def test_set_requires_a_non_null_goal_response(tmp_path: Path) -> None:
    with pytest.raises(CodexGoalError, match="expected an object"):
        set_goal("thread-fixture", "An objective", command=_server(tmp_path, "absent"))


def test_invalid_updates_are_refused_before_starting_transport(tmp_path: Path) -> None:
    command = _server(tmp_path)
    with pytest.raises(ValueError, match="provide an objective"):
        set_goal("thread-fixture", command=command)
    with pytest.raises(ValueError, match="must not be blank"):
        set_goal("thread-fixture", "  ", command=command)
    with pytest.raises(ValueError, match="unsupported"):
        set_goal("thread-fixture", status="invented", command=command)
    for budget in (0, -1, True):
        with pytest.raises(ValueError, match="positive integer"):
            set_goal("thread-fixture", token_budget=budget, command=command)
    assert not (tmp_path / "calls.jsonl").exists()


def test_timeout_reaps_transport_descendants(tmp_path: Path) -> None:
    started = time.monotonic()
    with pytest.raises(CodexGoalError, match="timed out"):
        get_goal("thread-fixture", _server(tmp_path, "hang"), timeout=0.5)
    assert time.monotonic() - started < 5
    pid = int((tmp_path / "child.pid").read_text())
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
            state = Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1]
        except (ProcessLookupError, FileNotFoundError):
            return
        if state == "Z":
            return
        time.sleep(0.01)
    pytest.fail(f"timed-out transport child {pid} remains running")


def test_missing_transport_uses_cross_language_os_error() -> None:
    with pytest.raises(
        CodexGoalError,
        match=r"cannot start Codex goal transport: No such file or directory \(os error 2\)",
    ):
        get_goal("thread-fixture", ("/definitely/missing/codex-goal-transport",))


def test_cli_writes_one_json_result(
    tmp_path: Path, capsys: pytest.CaptureFixture[str],
) -> None:
    assert _main([
        "--command-json", json.dumps(_server(tmp_path)), "get", "thread-fixture",
    ]) == 0
    output = capsys.readouterr()
    decoded: object = json.loads(output.out)
    assert as_mapping(decoded, "CLI result")["threadId"] == "thread-fixture"
    assert output.err == ""


def test_cli_failure_is_nonzero_and_has_no_fake_success_json(
    tmp_path: Path, capsys: pytest.CaptureFixture[str],
) -> None:
    assert _main([
        "--command-json", json.dumps(_server(tmp_path, "error")), "get", "thread-fixture",
    ]) == 1
    output = capsys.readouterr()
    assert output.out == ""
    assert "unknown goal method" in output.err

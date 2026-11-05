"""Output subscriptions discover a usable socket through the Herdr CLI."""

from __future__ import annotations

import json
import subprocess
from collections.abc import Sequence

import pytest

from agentctl.client import HerdrClient
from agentctl.errors import HerdrUnavailable


class Runner:
    def __init__(self, *responses: tuple[int, str]) -> None:
        self.responses = list(responses)
        self.calls: list[list[str]] = []

    def __call__(self, command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(list(command))
        code, stdout = self.responses.pop(0)
        return subprocess.CompletedProcess(command, code, stdout, "fixture diagnostics")


def _status(**fields: object) -> str:
    document: dict[str, object] = {
        "running": True, "compatible": True, "socket": "/runtime/server/events.sock",
    }
    document.update(fields)
    return json.dumps(document)


def test_event_socket_uses_herdr_discovery_and_refreshes_after_server_change() -> None:
    runner = Runner((0, _status()), (0, _status(socket="/runtime/replacement/events.sock")))
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner,
                        environ={"HERDR_SOCKET": "/tmp/unverified-caller.sock"})
    assert client.event_socket() == "/runtime/server/events.sock"
    assert client.event_socket() == "/runtime/replacement/events.sock"
    assert runner.calls == [
        ["fixture-herdr", "status", "server", "--json"],
        ["fixture-herdr", "status", "server", "--json"],
    ]


@pytest.mark.parametrize(("field", "value"), [
    ("running", False), ("running", None), ("running", 1), ("running", "true"),
    ("compatible", False), ("compatible", None), ("compatible", 1), ("compatible", "true"),
])
def test_event_socket_requires_positive_running_and_compatible_status(field: str, value: object) -> None:
    runner = Runner((0, _status(**{field: value})))
    with pytest.raises(HerdrUnavailable, match="running, compatible"):
        HerdrClient(run=runner).event_socket()
    assert runner.calls == [["herdr", "status", "server", "--json"]]


@pytest.mark.parametrize("socket", ["relative/events.sock", "", "/runtime/invalid\0socket", None, 17, {}])
def test_event_socket_rejects_relative_or_malformed_socket(socket: object) -> None:
    runner = Runner((0, _status(socket=socket)))
    with pytest.raises(HerdrUnavailable, match="invalid Herdr server status"):
        HerdrClient(run=runner).event_socket()


@pytest.mark.parametrize("document", [
    "not json", "", "null", "[]", "true", '{"running": true, "compatible": true}',
    '{"result": {"running": true, "compatible": true, "socket": "/runtime/server.sock"}}',
])
def test_event_socket_rejects_non_json_missing_fields_and_unexpected_status_shape(document: str) -> None:
    runner = Runner((0, document))
    with pytest.raises(HerdrUnavailable, match="invalid Herdr server status"):
        HerdrClient(run=runner).event_socket()


def test_event_socket_refuses_failed_status_even_with_success_shaped_stdout() -> None:
    runner = Runner((7, _status()))
    with pytest.raises(HerdrUnavailable, match="cannot discover"):
        HerdrClient(run=runner).event_socket()
    assert len(runner.calls) == 1


@pytest.mark.parametrize("error", [OSError("cannot execute"), subprocess.TimeoutExpired("herdr", 30)])
def test_event_socket_reports_command_failure_without_connecting(error: Exception) -> None:
    def runner(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        assert list(command) == ["herdr", "status", "server", "--json"]
        raise error

    with pytest.raises(HerdrUnavailable):
        HerdrClient(run=runner).event_socket()

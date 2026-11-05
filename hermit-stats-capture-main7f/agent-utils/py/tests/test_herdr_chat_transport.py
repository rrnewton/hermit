"""Real command boundaries and fake HTTP cover the public Chat transport contract."""

from __future__ import annotations

import hashlib
import io
import json
import os
import selectors
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Sequence
from email.message import Message
from pathlib import Path
from typing import IO, cast
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request

import pytest

import agentctl.chat as chat_module
from agentctl.agent import Target
from agentctl.chat import (
    Bridge, CommandTransport, Config, GoogleChatTransport,
    _read, _run_command, _write, submit_reply,
)
from agentctl.client import AgentPaneInfo, GuardedInput, HerdrClient, Pane
from agentctl.errors import AgentDeliveryError, HerdrUnavailable

_SPACE = "spaces/test"
_THREAD = "spaces/test/threads/thread-one"
_REQUEST_ID = "f17fb68a-5597-49a9-a1ab-d14b26331b0e"
_START = "2026-01-01T00:00:00Z"


class Http:
    def __init__(self, *responses: dict[str, object]) -> None:
        self.responses = list(responses)
        self.requests: list[Request] = []

    def __call__(self, request: Request, *, timeout: float) -> io.BytesIO:
        assert timeout == 45
        self.requests.append(request)
        return io.BytesIO(json.dumps(self.responses.pop(0)).encode())


def _poll(cursor: str | None = None) -> dict[str, object]:
    return {"action": "poll", "space": _SPACE, "after": _START, "cursor": cursor}


def _send() -> dict[str, object]:
    return {"action": "send", "space": _SPACE, "thread": _THREAD,
            "request_id": _REQUEST_ID, "text": "[fixture-agent] Done: π\nSecond line."}


def test_rest_poll_encodes_filter_and_opaque_page_token(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http({"messages": [{
        "name": _SPACE + "/messages/message-one", "text": "Please inspect",
        "sender": {"name": "users/owner", "type": "HUMAN"},
        "thread": {"name": _THREAD}, "createTime": "2026-01-01T00:01:00.123456Z",
    }], "nextPageToken": "next+/= cursor"})
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "fixture-access-token")
    cursor = 'opaque+/= token & filter="different"'
    result = GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")(_poll(cursor))
    request = http.requests[0]
    url = urlsplit(request.full_url)
    assert (url.scheme, url.netloc, url.path) == (
        "https", "chat.googleapis.com", "/v1/spaces/test/messages",
    )
    assert parse_qs(url.query) == {
        "pageSize": ["100"], "orderBy": ["createTime asc"],
        "filter": [f'createTime > "{_START}"'], "pageToken": [cursor],
    }
    assert request.get_method() == "GET"
    assert request.data is None
    assert request.get_header("Authorization") == "Bearer fixture-access-token"
    assert result == {"messages": [{
        "id": _SPACE + "/messages/message-one", "text": "Please inspect",
        "sender": "users/owner", "thread": _THREAD,
        "created_at": "2026-01-01T00:01:00.123456Z",
    }], "cursor": "next+/= cursor"}


def test_rest_send_preserves_text_thread_and_retry_identity(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    response: dict[str, object] = {"name": _SPACE + "/messages/reply"}
    http = Http(response, response)
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "fixture-access-token")
    transport = GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")
    assert transport(_send()) == transport(_send()) == {"id": response["name"]}
    first, second = http.requests
    assert first.full_url == second.full_url
    assert first.data == second.data
    assert first.get_method() == "POST"
    assert parse_qs(urlsplit(first.full_url).query) == {
        "requestId": [_REQUEST_ID], "messageReplyOption": ["REPLY_MESSAGE_OR_FAIL"],
    }
    assert isinstance(first.data, bytes)
    assert json.loads(first.data) == {
        "text": _send()["text"], "thread": {"name": _THREAD},
    }
    assert first.get_header("Content-type") == "application/json"


@pytest.mark.parametrize("token", ["secret-token\nhelper diagnostic", "secret-token\r", "secret-token π"])
def test_malformed_token_is_not_echoed_in_diagnostics(
    token: str, monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http()
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", token)
    with pytest.raises(ValueError, match="ASCII without whitespace") as error:
        GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")(_poll())
    assert "secret-token" not in str(error.value)
    assert not http.requests


def test_misspelled_authority_option_is_refused() -> None:
    with pytest.raises(ValueError, match="unknown chat configuration"):
        Config.parse({"agent_naem": "coordinator"})


def test_token_command_refreshes_on_every_page_and_send(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    counter = tmp_path / "token-counter"
    script = (
        "from pathlib import Path; import sys; p=Path(sys.argv[1]); "
        "n=int(p.read_text())+1 if p.exists() else 1; "
        "p.write_text(str(n)); print('  fixture-token-'+str(n)+'  ')"
    )
    command = (sys.executable, "-c", script, str(counter))
    http = Http({"messages": [], "nextPageToken": "next"},
                {"messages": []}, {"name": _SPACE + "/messages/reply"})
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "stale-environment-token")
    transport = GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN", command)
    transport(_poll())
    transport(_poll("next"))
    transport(_send())
    assert counter.read_text() == "3"
    assert [request.get_header("Authorization") for request in http.requests] == [
        "Bearer fixture-token-1", "Bearer fixture-token-2", "Bearer fixture-token-3",
    ]


@pytest.mark.parametrize("mode", ["failure", "empty"])
def test_failed_token_refresh_never_uses_stale_environment_credentials(
    monkeypatch: pytest.MonkeyPatch, mode: str,
) -> None:
    http = Http()
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "stale-environment-token")
    script = "import sys; print('credential-detail', file=sys.stderr); sys.exit(7)" if mode == "failure" else "print('')"
    transport = GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN", (sys.executable, "-c", script))
    with pytest.raises(ValueError) as caught:
        transport(_poll())
    assert "credential-detail" not in str(caught.value)
    assert http.requests == []


def test_http_error_is_propagated_for_durable_retry(monkeypatch: pytest.MonkeyPatch) -> None:
    def fail(request: Request, *, timeout: float) -> io.BytesIO:
        raise HTTPError(request.full_url, 429, "rate limited", Message(), None)

    monkeypatch.setattr(chat_module, "urlopen", fail)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "fixture-access-token")
    with pytest.raises(HTTPError) as caught:
        GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")(_send())
    assert caught.value.code == 429
    assert _REQUEST_ID in caught.value.url


def test_rest_rejects_invalid_boundaries_before_http(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http()
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "fixture-access-token")
    transport = GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")
    for key, value in (("space", "spaces/test/../other"),
                       ("thread", "spaces/other/threads/thread-one"),
                       ("request_id", "not-a-uuid")):
        request = _send()
        request[key] = value
        with pytest.raises(ValueError):
            transport(request)
    request = _poll()
    request["cursor"] = 37
    with pytest.raises(ValueError, match="cursor"):
        transport(request)
    assert http.requests == []


def test_rest_rejects_reply_resource_from_another_space(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    http = Http({"name": "spaces/other/messages/reply"})
    monkeypatch.setattr(chat_module, "urlopen", http)
    monkeypatch.setenv("TEST_CHAT_ACCESS_TOKEN", "fixture-access-token")
    with pytest.raises(ValueError, match="outside"):
        GoogleChatTransport("TEST_CHAT_ACCESS_TOKEN")(_send())


def test_command_adapter_preserves_literal_arguments_and_request(tmp_path: Path) -> None:
    marker = tmp_path / "must-not-exist"
    literal = f"$(touch {marker}); `echo substituted`\nspace and π"
    script = "import json,sys; print(json.dumps({'args':sys.argv[1:], 'request':json.load(sys.stdin)}))"
    request: dict[str, object] = {"action": "send", "text": literal}
    response = CommandTransport((sys.executable, "-c", script, literal))(request)
    assert response == {"args": [literal], "request": request}
    assert not marker.exists()


@pytest.mark.parametrize(
    ("script", "error", "message"),
    [
        ("import sys; print('adapter detail',file=sys.stderr); sys.exit(7)", ValueError, "exited 7"),
        ("print('not-json')", ValueError, "Expecting value"),
        ("print('[]')", TypeError, "expected an object"),
    ],
)
def test_command_adapter_failures_are_not_empty_successes(
    script: str, error: type[Exception], message: str,
) -> None:
    with pytest.raises(error, match=message):
        CommandTransport((sys.executable, "-c", script))(_poll())


def _assert_command_child_gone(pid: int, outcome: str) -> None:
    deadline = time.monotonic() + 2
    state = "unknown"
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
            state = Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1]
        except (ProcessLookupError, FileNotFoundError):
            return
        time.sleep(0.01)
    pytest.fail(f"{outcome} adapter child {pid} remains in /proc with state {state}")


def _wait_for_process_identities(
    path: Path, count: int, description: str, *, timeout: float = 2,
    producer: threading.Thread | None = None,
    producer_outcome: Callable[[], str] | None = None,
) -> tuple[int, ...]:
    """Wait until a fixture publishes its complete, parseable PID payload."""
    deadline = time.monotonic() + timeout
    observed = "missing"
    while time.monotonic() < deadline:
        try:
            payload = path.read_text()
        except FileNotFoundError:
            observed = "missing"
        else:
            values = payload.split()
            try:
                identities = tuple(int(value) for value in values)
            except ValueError:
                observed = repr(payload)
            else:
                if len(identities) == count and all(pid > 0 for pid in identities):
                    return identities
                observed = repr(payload)
        if producer is not None and not producer.is_alive():
            outcome = producer_outcome() if producer_outcome is not None else "unknown"
            pytest.fail(
                f"{description} worker completed before publishing {count} process "
                f"identities ({outcome}); last payload was {observed}"
            )
        time.sleep(0.005)
    producer_status = ""
    if producer is not None:
        outcome = producer_outcome() if producer_outcome is not None else "unknown"
        producer_status = f"; worker={'running' if producer.is_alive() else 'complete'} ({outcome})"
    pytest.fail(
        f"{description} did not publish {count} process identities; "
        f"last payload was {observed}{producer_status}"
    )


def test_command_supervisor_reaps_nested_descendants_before_return(tmp_path: Path) -> None:
    """An outer subreaper exposes zombies which would otherwise be hidden by PID 1."""
    pids = tmp_path / "descendants"
    package_parent = Path(chat_module.__file__).resolve().parent.parent
    probe = r'''
import ctypes
import os
import pathlib
import sys
import time

sys.path.insert(0, sys.argv[2])
from agentctl.chat import _run_command

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
pids = pathlib.Path(sys.argv[1])
grandchild = "import time; time.sleep(60)"
child = (
    "import os,pathlib,subprocess,sys,time; "
    "nested=subprocess.Popen([sys.executable,'-c',sys.argv[2]]); "
    "pathlib.Path(sys.argv[1]).write_text(f'{os.getpid()} {nested.pid}'); "
    "time.sleep(60)"
)
leader = (
    "import pathlib,subprocess,sys,time\n"
    "path=pathlib.Path(sys.argv[1])\n"
    "subprocess.Popen([sys.executable,'-c',sys.argv[2],sys.argv[1],sys.argv[3]])\n"
    "deadline=time.monotonic()+2\n"
    "while not path.exists() and time.monotonic()<deadline:\n"
    "    time.sleep(0.01)\n"
    "if not path.exists():\n"
    "    raise RuntimeError('descendant did not start')\n"
)
result = _run_command(
    (sys.executable, "-c", leader, str(pids), child, grandchild), timeout=5,
)
if result.returncode != 0:
    raise SystemExit(f"leader returned {result.returncode}: {result.stderr}")
descendants = [int(value) for value in pids.read_text().split()]
remaining = [
    (pid, pathlib.Path(f"/proc/{pid}/stat").read_text().rpartition(") ")[2][:1])
    for pid in descendants
    if pathlib.Path(f"/proc/{pid}").exists()
]
while True:
    try:
        pid, _ = os.waitpid(-1, os.WNOHANG)
    except ChildProcessError:
        break
    if pid == 0:
        break
if remaining:
    raise SystemExit(f"unreaped descendants: {remaining}")
'''
    completed = subprocess.run(
        (sys.executable, "-c", probe, str(pids), str(package_parent)),
        text=True, capture_output=True, timeout=10, check=False,
    )
    assert completed.returncode == 0, completed.stderr


@pytest.mark.parametrize("stopped", ["none", "supervisor", "anchor", "both"])
def test_command_parent_death_reaps_supervisor_and_adapter_group(
    tmp_path: Path, stopped: str,
) -> None:
    """Only the isolated probe becomes a subreaper; the bridge parent never does."""
    pids = tmp_path / "parent-death-descendants"
    package_parent = Path(chat_module.__file__).resolve().parent.parent
    probe = r'''
import ctypes
import os
import pathlib
import signal
import subprocess
import sys
import time

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
pids = pathlib.Path(sys.argv[1])
package_parent = sys.argv[2]
caller_program = r"""
import pathlib
import sys
sys.path.insert(0, sys.argv[2])
from agentctl.chat import _run_command
pids = pathlib.Path(sys.argv[1])
child = "import os,time; os.close(0); os.close(1); os.close(2); time.sleep(60)"
adapter = (
    "import os,pathlib,subprocess,sys,time; "
    "nested=subprocess.Popen([sys.executable,'-c',sys.argv[2]]); "
    "pathlib.Path(sys.argv[1]).write_text(f'{os.getpid()} {nested.pid}'); "
    "time.sleep(60)"
)
_run_command((sys.executable, "-c", adapter, str(pids), child), timeout=60)
"""
caller = subprocess.Popen(
    (sys.executable, "-c", caller_program, str(pids), package_parent),
)
deadline = time.monotonic() + 5
supervisors = []
while time.monotonic() < deadline:
    if pids.exists():
        try:
            adapter_pid = int(pids.read_text().split()[0])
            status = pathlib.Path(f"/proc/{adapter_pid}/status").read_text()
            supervisors = [int(next(
                line.split()[1] for line in status.splitlines() if line.startswith("PPid:")
            ))]
        except FileNotFoundError:
            supervisors = []
        if len(supervisors) == 1 and supervisors[0] != 0:
            break
    time.sleep(0.01)
if not pids.exists() or len(supervisors) != 1 or supervisors[0] == 0:
    outcome = caller.poll()
    caller.kill()
    caller.wait(timeout=2)
    raise SystemExit(
        f"caller did not expose one live supervisor and adapter group: "
        f"pids={pids.exists()} supervisors={supervisors} caller={outcome}"
    )
supervisor = supervisors[0]
adapter_pids = [int(value) for value in pids.read_text().split()]
anchor = os.getpgid(adapter_pids[0])
caller_pid = caller.pid
supervisor_status = pathlib.Path(f"/proc/{supervisor}/status").read_text()
if supervisor_status.split("PPid:\t", 1)[1].splitlines()[0] != str(caller_pid):
    raise SystemExit("adapter supervisor was not parented by the _run_command caller")
stopped = sys.argv[3]
frozen = ([supervisor] if stopped == "supervisor" else [anchor]
          if stopped == "anchor" else [supervisor, anchor] if stopped == "both" else [])
for pid in frozen:
    os.kill(pid, signal.SIGSTOP)
for pid in frozen:
    deadline = time.monotonic() + 2
    while pathlib.Path(f"/proc/{pid}/stat").read_bytes().rpartition(b") ")[2][:1] != b"T":
        if time.monotonic() >= deadline:
            raise SystemExit(f"could not freeze {pid}")
        time.sleep(0.005)
os.kill(caller_pid, signal.SIGKILL)
caller.wait(timeout=2)

status = None
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    try:
        reaped, candidate = os.waitpid(supervisor, os.WNOHANG)
    except ChildProcessError:
        time.sleep(0.01)
        continue
    if reaped == supervisor:
        status = candidate
        break
    time.sleep(0.01)
if status is None:
    for pid in [supervisor, *adapter_pids]:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    raise SystemExit("parent death did not terminate and reap the supervisor")
if os.waitstatus_to_exitcode(status) != 128 + signal.SIGTERM:
    raise SystemExit(f"supervisor parent-death status was {os.waitstatus_to_exitcode(status)}")
remaining = [
    pid for pid in [caller_pid, supervisor, anchor, *adapter_pids]
    if pathlib.Path(f"/proc/{pid}").exists()
]
if remaining:
    raise SystemExit(f"parent-death cleanup left /proc entries: {remaining}")
'''
    completed = subprocess.run(
        (sys.executable, "-c", probe, str(pids), str(package_parent), stopped),
        text=True, capture_output=True, timeout=12, check=False,
    )
    assert completed.returncode == 0, completed.stderr


@pytest.mark.parametrize("close_stdio", [False, True])
def test_command_success_preserves_leader_response_and_terminates_descendants(
    tmp_path: Path, close_stdio: bool,
) -> None:
    pid_path = tmp_path / "child.pid"
    child_script = (
        "import os,time; os.close(0); os.close(1); os.close(2); time.sleep(60)"
        if close_stdio else "import time; time.sleep(60)"
    )
    script = (
        "import os,pathlib,sys; "
        "child=os.posix_spawn(sys.executable,[sys.executable,'-c',sys.argv[2]],os.environ); "
        "pathlib.Path(sys.argv[1]).write_text(str(child)); "
        "print('leader response',flush=True)"
    )
    started = time.monotonic()
    result = _run_command(
        (sys.executable, "-c", script, str(pid_path), child_script), timeout=5,
    )
    assert time.monotonic() - started < 3
    assert result.returncode == 0
    assert result.stdout == "leader response\n"
    assert result.stderr == ""
    _assert_command_child_gone(int(pid_path.read_text()), "successful")


@pytest.mark.parametrize("disposition", ["ignored", "no-cldwait"])
def test_command_refuses_autoreap_before_stopped_double_pidfd_race(
    tmp_path: Path, disposition: str,
) -> None:
    """Unsafe inherited SIGCHLD state must grant no supervisor or adapter authority."""
    python_path = str(Path(__file__).resolve().parents[1])
    setup = (
        "signal.signal(signal.SIGCHLD,signal.SIG_IGN)"
        if disposition == "ignored"
        else """
class SigSet(ctypes.Structure):
    _fields_=[('values',ctypes.c_ulong*16)]
class SigAction(ctypes.Structure):
    _fields_=[('handler',ctypes.c_void_p),('mask',SigSet),('flags',ctypes.c_int),('restorer',ctypes.c_void_p)]
action=SigAction()
action.handler=ctypes.c_void_p(0)
libc=ctypes.CDLL(None,use_errno=True)
assert libc.sigemptyset(ctypes.byref(action.mask)) == 0
action.flags=2
assert libc.sigaction(signal.SIGCHLD,ctypes.byref(action),None) == 0
"""
    )
    marker = tmp_path / f"command-autoreap-{disposition}"
    script = f"""
import ctypes,os,pathlib,signal,subprocess,sys,time
{setup}
import agentctl.chat as chat
original_open=os.pidfd_open
original_popen=subprocess.Popen
spawned=[]
handles=[]
nonself_opens=[]
def stopped_popen(*args,**kwargs):
    process=original_popen(*args,**kwargs)
    descriptor=original_open(process.pid)
    spawned.append(process)
    handles.append(descriptor)
    signal.pidfd_send_signal(descriptor,signal.SIGSTOP)
    deadline=time.monotonic()+2
    while pathlib.Path(f'/proc/{{process.pid}}/stat').read_text().rpartition(') ')[2][:1] not in ('T','t'):
        if time.monotonic() >= deadline:
            raise AssertionError('supervisor did not stop')
        time.sleep(.005)
    return process
def exhaust_after_spawn(pid,flags=0):
    if pid == os.getpid():
        return original_open(pid,flags)
    nonself_opens.append(pid)
    raise OSError(24,'combined stopped-supervisor descriptor exhaustion')
chat.subprocess.Popen=stopped_popen
chat.os.pidfd_open=exhaust_after_spawn
marker=pathlib.Path({str(marker)!r})
try:
    try:
        chat._run_command((sys.executable,'-c',"import pathlib,sys;pathlib.Path(sys.argv[1]).touch()",str(marker)),timeout=5)
    except ValueError as exc:
        assert 'SIGCHLD=SIG_DFL' in str(exc),str(exc)
    else:
        raise AssertionError('unsafe SIGCHLD state was accepted')
    assert not spawned,spawned
    assert not nonself_opens,nonself_opens
    assert not marker.exists()
finally:
    chat.os.pidfd_open=original_open
    chat.subprocess.Popen=original_popen
    for process,descriptor in zip(spawned,handles):
        try:
            signal.pidfd_send_signal(descriptor,signal.SIGCONT)
            signal.pidfd_send_signal(descriptor,signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=2)
        except (ChildProcessError,subprocess.TimeoutExpired):
            pass
        os.close(descriptor)
print('pre-spawn-refusal-ok')
"""
    environment = dict(os.environ)
    environment["PYTHONPATH"] = python_path + os.pathsep + environment.get("PYTHONPATH", "")
    result = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True,
        env=environment, timeout=15, check=False,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "pre-spawn-refusal-ok"
    assert not marker.exists()


@pytest.mark.parametrize("returncode", [0, 7, -signal.SIGKILL])
def test_command_supervisor_preserves_status_and_teardown_signals_with_default_sigchld(
    tmp_path: Path, returncode: int,
) -> None:
    chat_module._require_waitable_sigchld_children("command status regression")
    pid_path = tmp_path / "default-signal-child"
    script = (
        "import os,pathlib,signal,sys; "
        "child=os.posix_spawn(sys.executable,[sys.executable,'-c','import time; time.sleep(60)'],os.environ); "
        "pathlib.Path(sys.argv[1]).write_text(str(child)); "
        "print('signal-safe',flush=True); "
        "print('signal-stderr',file=sys.stderr,flush=True); "
        "code=int(sys.argv[2]); "
        "os.kill(os.getpid(),-code) if code < 0 else sys.exit(code)"
    )
    result = _run_command(
        (sys.executable, "-c", script, str(pid_path), str(returncode)), timeout=5,
    )
    assert result.returncode == returncode
    assert result.stdout == "signal-safe\n" and result.stderr == "signal-stderr\n"
    _assert_command_child_gone(int(pid_path.read_text()), "default-signal")


@pytest.mark.parametrize("error_number", [38, 24])  # ENOSYS and EMFILE
def test_command_without_pidfd_fails_before_spawning(
    monkeypatch: pytest.MonkeyPatch, error_number: int,
) -> None:
    def unavailable(pid: int, flags: int = 0) -> int:
        raise OSError(error_number, os.strerror(error_number))

    def forbidden_spawn(*args: object, **kwargs: object) -> None:
        pytest.fail("no process may start when containment descriptors are unavailable")

    monkeypatch.setattr(os, "pidfd_open", unavailable)
    monkeypatch.setattr(subprocess, "Popen", forbidden_spawn)
    with pytest.raises(RuntimeError, match="process descriptor unavailable"):
        _run_command((sys.executable, "-c", "raise SystemExit(0)"), timeout=5)


def test_command_supervisor_pidfd_failure_retries_only_for_cleanup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = os.pidfd_open
    calls: list[int] = []

    def exhaust_once(pid: int, flags: int = 0) -> int:
        calls.append(pid)
        if len(calls) == 2:
            raise OSError(24, "injected process descriptor exhaustion")
        return original_open(pid, flags)

    marker = tmp_path / "must-not-start"
    monkeypatch.setattr(os, "pidfd_open", exhaust_once)
    with pytest.raises(OSError, match="descriptor exhaustion"):
        _run_command((sys.executable, "-c",
                      "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)),
                     timeout=5)
    assert calls[0] == os.getpid() and calls[1] == calls[2]
    assert not marker.exists()
    _assert_command_child_gone(calls[1], "preauthority descriptor exhaustion")


def test_command_repeated_double_pidfd_exhaustion_reaps_stopped_supervisors(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    chat_module._require_waitable_sigchld_children("command cleanup regression")
    original_open = os.pidfd_open
    original_pipe = os.pipe2
    original_popen = subprocess.Popen
    original_kill = os.kill
    before_fds = len(list(Path("/proc/self/fd").iterdir()))
    before_tasks = len(list(Path("/proc/self/task").iterdir()))
    marker = tmp_path / "must-not-start"
    processes: list[subprocess.Popen[bytes]] = []
    anchors: list[int] = []
    pipe_reads: dict[int, int] = {}
    pidfd_calls: list[int] = []
    signals: list[tuple[int, int]] = []

    def record_pipe(flags: int) -> tuple[int, int]:
        read_descriptor, write_descriptor = original_pipe(flags)
        pipe_reads[write_descriptor] = read_descriptor
        return read_descriptor, write_descriptor

    def exhaust_twice(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        pidfd_calls.append(pid)
        message = (
            "original command pidfd exhaustion"
            if len(pidfd_calls) % 2 else "cleanup retry pidfd exhaustion"
        )
        raise OSError(24, message)

    def record_signal(pid: int, signum: int) -> None:
        signals.append((pid, signum))
        original_kill(pid, signum)

    def stopped_popen(
        args: Sequence[str], *, stdin: int | IO[bytes] | None = None,
        stdout: int | IO[bytes] | None = None,
        stderr: int | IO[bytes] | None = None,
        start_new_session: bool = False, bufsize: int = -1,
        pass_fds: Sequence[int] = (),
    ) -> subprocess.Popen[bytes]:
        process = original_popen(
            args, stdin=stdin, stdout=stdout, stderr=stderr,
            start_new_session=start_new_session, bufsize=bufsize, pass_fds=pass_fds,
        )
        processes.append(process)
        identity_read = pipe_reads[int(args[3])]
        selector = selectors.DefaultSelector()
        try:
            selector.register(identity_read, selectors.EVENT_READ)
            if not selector.select(2):
                pytest.fail("command supervisor did not publish its pre-ACK anchor")
            publication = json.loads(os.read(identity_read, 4096).splitlines()[0])
        finally:
            selector.close()
        assert isinstance(publication, dict) and isinstance(publication.get("pid"), int)
        anchors.append(int(publication["pid"]))
        os.kill(process.pid, signal.SIGSTOP)
        deadline = time.monotonic() + 2
        while True:
            state = Path(f"/proc/{process.pid}/stat").read_text().rpartition(") ")[2][:1]
            if state in ("T", "t"):
                return process
            if time.monotonic() >= deadline:
                pytest.fail("command supervisor did not stop before pidfd exhaustion")
            time.sleep(0.005)

    monkeypatch.setattr(os, "pidfd_open", exhaust_twice)
    monkeypatch.setattr(os, "pipe2", record_pipe)
    monkeypatch.setattr(os, "kill", record_signal)
    monkeypatch.setattr(subprocess, "Popen", stopped_popen)
    for _ in range(10):
        with pytest.raises(OSError, match="original command pidfd exhaustion"):
            _run_command(
                (
                    sys.executable, "-c",
                    "import pathlib,sys;pathlib.Path(sys.argv[1]).touch()", str(marker),
                ),
                timeout=5,
            )
    assert pidfd_calls == [pid for process in processes for pid in (process.pid, process.pid)]
    assert signals == [
        item for process in processes for item in (
            (process.pid, signal.SIGSTOP),
            (process.pid, signal.SIGCONT),
            (process.pid, signal.SIGKILL),
        )
    ]
    assert all(process.returncode == -signal.SIGKILL for process in processes)
    assert all(not Path(f"/proc/{process.pid}").exists() for process in processes)
    assert len(anchors) == len(processes)
    deadline = time.monotonic() + 2
    while any(Path(f"/proc/{pid}").exists() for pid in anchors) and time.monotonic() < deadline:
        time.sleep(0.01)
    assert all(not Path(f"/proc/{pid}").exists() for pid in anchors)
    assert not marker.exists()
    assert len(list(Path("/proc/self/fd").iterdir())) == before_fds
    assert len(list(Path("/proc/self/task").iterdir())) == before_tasks


def test_command_preauthority_cleanup_fault_is_not_excused(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = os.pidfd_open
    original_cleanup = chat_module._cleanup_unpinned_direct_child

    def exhaust(pid: int, flags: int = 0) -> int:
        if pid == os.getpid():
            return original_open(pid, flags)
        raise OSError(24, "original command pidfd exhaustion")

    def cleanup_then_fail(
        process: subprocess.Popen[bytes], expected: chat_module._CommandAnchorIdentity,
        context: str, *, timeout: float = 5.0,
    ) -> None:
        original_cleanup(process, expected, context, timeout=timeout)
        raise RuntimeError("synthetic preauthority cleanup proof failure")

    monkeypatch.setattr(os, "pidfd_open", exhaust)
    monkeypatch.setattr(chat_module, "_cleanup_unpinned_direct_child", cleanup_then_fail)
    with pytest.raises(RuntimeError, match="cleanup proof failure") as caught:
        _run_command((sys.executable, "-c", "raise SystemExit(0)"), timeout=5)
    assert isinstance(caught.value.__context__, OSError)


def test_command_anchor_pidfd_failure_refuses_authority_and_reaps_helpers(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_open = os.pidfd_open
    calls: list[int] = []

    def exhaust_anchor(pid: int, flags: int = 0) -> int:
        calls.append(pid)
        if len(calls) == 3:
            raise OSError(24, "injected anchor descriptor exhaustion")
        return original_open(pid, flags)

    marker = tmp_path / "must-not-start"
    monkeypatch.setattr(os, "pidfd_open", exhaust_anchor)
    with pytest.raises(OSError, match="anchor descriptor exhaustion"):
        _run_command((sys.executable, "-c",
                      "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)),
                     timeout=5)
    assert len(calls) == 3 and not marker.exists()
    for pid in calls[1:]:
        _assert_command_child_gone(pid, "preauthority anchor descriptor exhaustion")


def test_command_missing_final_result_never_uses_popen_success(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_read_result = chat_module._read_command_result
    consumed = False

    def lost_result(descriptor: int) -> int:
        nonlocal consumed
        if not consumed:
            # Consume the real successful status, then expose EOF to simulate
            # a missing private result. Popen itself still observes exit zero.
            assert original_read_result(descriptor) == 0
            consumed = True
        return original_read_result(descriptor)

    monkeypatch.setattr(chat_module, "_read_command_result", lost_result)
    with pytest.raises(RuntimeError, match="containment could not prove"):
        _run_command((sys.executable, "-c", "print('must not report success')"), timeout=5)
    assert consumed


@pytest.mark.parametrize("failure_at", [1, 2, 3, 4])
def test_command_control_pipe_failure_releases_prior_descriptors(
    monkeypatch: pytest.MonkeyPatch, failure_at: int,
) -> None:
    original_pipe = os.pipe2
    calls = 0
    descriptors_before = set(Path("/proc/self/fd").iterdir())

    def exhausted(flags: int) -> tuple[int, int]:
        nonlocal calls
        calls += 1
        if calls == failure_at:
            raise OSError(24, "injected pipe descriptor exhaustion")
        return original_pipe(flags)

    monkeypatch.setattr(os, "pipe2", exhausted)
    with pytest.raises(OSError, match="pipe descriptor exhaustion"):
        _run_command((sys.executable, "-c", "raise SystemExit(0)"), timeout=5)
    assert set(Path("/proc/self/fd").iterdir()) == descriptors_before


@pytest.mark.parametrize("wire", [
    b"", b"{}\n", b'{"version":1,"returncode":7}',
    b'{"version":1,"returncode":7}\nextra',
    b'{"version":1,"returncode":7}\n{}\n',
    b'{"version":1,"returncode":7,"returncode":0}\n',
    b'{"version":1,"returncode":true}\n',
    b'{"version":1,"returncode":256}\n',
    b'{"version":1,"returncode":-65}\n',
    b'{"version":true,"returncode":0}\n',
    b'{"version":1,"returncode":0,"extra":0}\n',
    b"x" * 257,
])
def test_command_final_result_requires_one_exact_bounded_record(wire: bytes) -> None:
    reader, writer = os.pipe()
    try:
        os.write(writer, wire)
        os.close(writer)
        writer = -1
        with pytest.raises((ValueError, TypeError)):
            chat_module._read_command_result(reader)
    finally:
        os.close(reader)
        if writer >= 0:
            os.close(writer)


@pytest.mark.parametrize("wire", [b"{bad-json}\n", b'{"version":1', b"x" * 4097])
def test_command_malformed_publication_never_grants_adapter_authority(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, wire: bytes,
) -> None:
    original_publication = chat_module._read_command_publication
    identities: list[chat_module._CommandAnchorIdentity] = []
    supervisor_ids: list[int] = []

    def corrupt_publication(
        descriptor: int, supervisor_pid: int, deadline: float,
    ) -> chat_module._CommandAnchorIdentity:
        identity = original_publication(descriptor, supervisor_pid, deadline)
        identities.append(identity)
        supervisor_ids.append(supervisor_pid)
        reader, writer = os.pipe()
        os.write(writer, wire)
        os.close(writer)
        try:
            # Feed actual malformed/partial framing through the real parser.
            return original_publication(reader, supervisor_pid, deadline)
        finally:
            os.close(reader)

    marker = tmp_path / "must-not-start"
    monkeypatch.setattr(chat_module, "_read_command_publication", corrupt_publication)
    with pytest.raises(ValueError):
        _run_command((sys.executable, "-c",
                      "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)),
                     timeout=5)
    assert not marker.exists()
    for pid in [*supervisor_ids, *(item.pid for item in identities)]:
        _assert_command_child_gone(pid, "malformed handshake")


def test_command_emergency_census_ignores_non_utf8_unrelated_comm(tmp_path: Path) -> None:
    ready = tmp_path / "non-utf8-ready"
    script = r'''
import ctypes
import pathlib
import sys
import time
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(15, ctypes.c_char_p(b"p\xff)\n(\x80"), 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_NAME")
pathlib.Path(sys.argv[1]).write_text("ready")
time.sleep(60)
'''
    unrelated = subprocess.Popen((sys.executable, "-c", script, str(ready)))
    try:
        deadline = time.monotonic() + 2
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.005)
        assert ready.exists()
        nonexistent = chat_module._CommandAnchorIdentity(
            pid=1 << 30, starttime=1, ppid=1, pgrp=1 << 30,
            session=1 << 30, state="T",
        )
        assert chat_module._command_group_members(nonexistent) == ()
    finally:
        unrelated.kill()
        unrelated.wait(timeout=2)


def test_command_failure_preserves_leader_result_and_terminates_descendant(tmp_path: Path) -> None:
    pid_path = tmp_path / "child.pid"
    script = (
        "import os,pathlib,sys; "
        "child=os.posix_spawn(sys.executable,[sys.executable,'-c','import time; time.sleep(60)'],os.environ); "
        "pathlib.Path(sys.argv[1]).write_text(str(child)); "
        "print('failure detail',file=sys.stderr,flush=True); sys.exit(7)"
    )
    started = time.monotonic()
    result = _run_command((sys.executable, "-c", script, str(pid_path)), timeout=5)
    assert time.monotonic() - started < 3
    assert result.returncode == 7
    assert result.stdout == ""
    assert result.stderr == "failure detail\n"
    _assert_command_child_gone(int(pid_path.read_text()), "failed")


@pytest.mark.parametrize("signum", [signal.SIGTERM, signal.SIGKILL])
def test_command_preserves_signal_returncode(signum: signal.Signals) -> None:
    result = _run_command(
        (sys.executable, "-c", "import os,sys; os.kill(os.getpid(),int(sys.argv[1]))",
         str(signum.value)),
        timeout=5,
    )
    assert result.returncode == -signum
    assert result.stdout == ""
    assert result.stderr == ""


def test_command_timeout_terminates_descendants_holding_output_pipes(tmp_path: Path) -> None:
    pid_path = tmp_path / "child.pid"
    script = (
        "import pathlib,subprocess,sys,time; "
        "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
        "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); time.sleep(60)"
    )
    started = time.monotonic()
    with pytest.raises(subprocess.TimeoutExpired):
        _run_command((sys.executable, "-c", script, str(pid_path)), timeout=0.5)
    assert time.monotonic() - started < 5
    _assert_command_child_gone(int(pid_path.read_text()), "timed-out")


@pytest.mark.parametrize("cleanup", ["cooperative", "emergency", "census-error"])
def test_command_timeout_contains_group_when_supervisor_is_stopped(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, cleanup: str,
) -> None:
    """The old fallback killed only S's PGID, leaving its separately-sessioned C alive."""
    command_timeout = 5.0
    cleanup_grace = 8.0
    # How long past the command timeout the worker may take before the test
    # wakes the stopped supervisor to unblock it: a bound on hanging, not on
    # how fast containment must be. The supervisor treats SIGCONT as
    # cancellation and kills the group it supervises, so waking it sooner can
    # empty that group in the middle of the emergency census, which then reads
    # no members although containment was proceeding correctly.
    hang_guard = 60.0
    pid_path = tmp_path / "stopped-supervisor-pids"
    child_script = "import os,time; os.close(0); os.close(1); os.close(2); time.sleep(60)"
    adapter = (
        "import os,pathlib,subprocess,sys,time; "
        "child=subprocess.Popen([sys.executable,'-c',sys.argv[2]]); "
        "path=pathlib.Path(sys.argv[1]); temporary=path.with_suffix('.tmp'); "
        "temporary.write_text(f'{os.getpid()} {child.pid}'); temporary.replace(path); "
        "time.sleep(60)"
    )
    outcomes: list[BaseException] = []
    completions: list[subprocess.CompletedProcess[str]] = []
    censuses: list[tuple[chat_module._CommandAnchorIdentity, ...]] = []
    if cleanup != "cooperative":
        original_signal = signal.pidfd_send_signal
        original_census = chat_module._command_group_members
        dropped: set[int] = set()

        def unresponsive(descriptor: int, signum: int) -> None:
            if signum in (signal.SIGTERM, signal.SIGCONT) and signum not in dropped:
                dropped.add(signum)
                return
            original_signal(descriptor, signum)

        def observe_census(
            anchor: chat_module._CommandAnchorIdentity,
        ) -> tuple[chat_module._CommandAnchorIdentity, ...]:
            if cleanup == "emergency":
                # One census may exceed the nominal retry window on a large
                # host.  The containment proof still needs two snapshots.
                time.sleep(0.55)
            members = original_census(anchor)
            censuses.append(members)
            if cleanup == "census-error":
                raise OSError(5, "injected emergency census failure")
            return members

        monkeypatch.setattr(signal, "pidfd_send_signal", unresponsive)
        monkeypatch.setattr(chat_module, "_command_group_members", observe_census)

    def invoke() -> None:
        try:
            completions.append(_run_command(
                (sys.executable, "-c", adapter, str(pid_path), child_script),
                timeout=command_timeout,
            ))
        except BaseException as exc:
            outcomes.append(exc)

    def worker_outcome() -> str:
        if outcomes:
            return f"raised {type(outcomes[0]).__name__}: {outcomes[0]}"
        if completions:
            return f"returned {completions[0].returncode}"
        return "no result recorded"

    worker = threading.Thread(target=invoke)
    invocation_started = time.monotonic()
    worker.start()
    adapter_pid, child_pid = _wait_for_process_identities(
        pid_path, 2, "adapter", timeout=command_timeout + 2,
        producer=worker, producer_outcome=worker_outcome,
    )
    supervisor_pid = int(next(
        line.split()[1]
        for line in Path(f"/proc/{adapter_pid}/status").read_text().splitlines()
        if line.startswith("PPid:")
    ))
    os.kill(supervisor_pid, signal.SIGSTOP)
    worker_deadline = invocation_started + command_timeout + hang_guard
    worker.join(timeout=max(0, worker_deadline - time.monotonic()))
    if worker.is_alive():
        os.kill(supervisor_pid, signal.SIGCONT)
        worker.join(timeout=cleanup_grace)
    assert not worker.is_alive(), (
        f"stopped supervisor left command cleanup blocked ({worker_outcome()})"
    )
    assert not completions and len(outcomes) == 1 and isinstance(
        outcomes[0], subprocess.TimeoutExpired,
    ), f"unexpected command worker outcome: {worker_outcome()}"
    if cleanup == "emergency":
        assert len(censuses) >= 2
        assert all(member.state in ("T", "t", "Z") for member in censuses[-1])
        assert {adapter_pid, child_pid}.issubset({member.pid for member in censuses[-1]})
    elif cleanup == "census-error":
        assert len(censuses) == 1
    for pid, role in ((supervisor_pid, "supervisor"),
                      (adapter_pid, "adapter"), (child_pid, "descendant")):
        _assert_command_child_gone(pid, f"stopped-supervisor {role}")


@pytest.mark.parametrize(("descriptor", "name"), [(1, "stdout"), (2, "stderr")])
def test_command_output_is_bounded_before_json_parsing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, descriptor: int, name: str,
) -> None:
    monkeypatch.setattr(chat_module, "_MAX_COMMAND_STDOUT", 1024)
    monkeypatch.setattr(chat_module, "_MAX_COMMAND_STDERR", 1024)
    pid_path = tmp_path / "child.pid"
    script = (
        "import os,pathlib,subprocess,sys,time; "
        "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
        "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); "
        f"os.write({descriptor}, b'x' * 1025); time.sleep(60)"
    )
    with pytest.raises(ValueError, match=rf"{name} exceeds its byte limit"):
        _run_command((sys.executable, "-c", script, str(pid_path)), timeout=5)
    _assert_command_child_gone(int(pid_path.read_text()), "overflowed")


def test_command_output_read_failure_cleans_the_complete_group(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    pid_path = tmp_path / "read-failure-pids"
    script = (
        "import os,pathlib,subprocess,sys,time; "
        "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
        "pathlib.Path(sys.argv[1]).write_text(f'{os.getpid()} {child.pid}'); "
        "print('ready',flush=True); time.sleep(60)"
    )
    original_read = os.read
    failed = False

    def fail_output(descriptor: int, length: int) -> bytes:
        nonlocal failed
        if length == 65536 and not failed and pid_path.exists():
            failed = True
            raise OSError(5, "injected output read failure")
        return original_read(descriptor, length)

    monkeypatch.setattr(os, "read", fail_output)
    with pytest.raises(OSError, match="injected output read failure"):
        _run_command((sys.executable, "-c", script, str(pid_path)), timeout=5)
    assert failed
    for value in pid_path.read_text().split():
        _assert_command_child_gone(int(value), "output read failure")


def test_command_final_wait_failure_always_wakes_anchor_then_reaps_supervisor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    original_wait = cast(
        Callable[[subprocess.Popen[bytes], float | None], int],
        subprocess.Popen.wait,
    )
    original_signal = signal.pidfd_send_signal
    original_open = os.pidfd_open
    handles: list[tuple[int, int]] = []
    killed = False
    failed = False
    woke_after_failure = False

    def remember_open(pid: int, flags: int = 0) -> int:
        descriptor = original_open(pid, flags)
        handles.append((pid, descriptor))
        return descriptor

    def record_signal(descriptor: int, signum: int) -> None:
        nonlocal killed, woke_after_failure
        if len(handles) >= 3 and descriptor == handles[1][1] and signum == signal.SIGKILL:
            killed = True
        if (len(handles) >= 3 and descriptor == handles[2][1]
                and signum == signal.SIGCONT and failed):
            woke_after_failure = True
        original_signal(descriptor, signum)

    def wait_failure(process: subprocess.Popen[bytes], timeout: float | None = None) -> int:
        nonlocal failed
        if killed and not failed:
            failed = True
            assert timeout is not None
            raise subprocess.TimeoutExpired("injected final supervisor wait", timeout)
        return original_wait(process, timeout)

    monkeypatch.setattr(os, "pidfd_open", remember_open)
    monkeypatch.setattr(signal, "pidfd_send_signal", record_signal)
    monkeypatch.setattr(subprocess.Popen, "wait", wait_failure)
    pid_path = tmp_path / "wait-failure-adapter"
    command = (sys.executable, "-c", "import os,pathlib,sys,time; "
               "pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)",
               str(pid_path))
    with pytest.raises(subprocess.TimeoutExpired):
        _run_command(command, timeout=0.5)
    assert failed and woke_after_failure
    for pid in [*(pid for pid, _ in handles[1:]), int(pid_path.read_text())]:
        _assert_command_child_gone(pid, "final wait failure")


@pytest.mark.parametrize("failure", ["lifeline-read", "supervisor-signal"])
def test_active_command_anchor_fault_always_kills_its_group(
    monkeypatch: pytest.MonkeyPatch, failure: str,
) -> None:
    from agentctl import _command_anchor as anchor_module

    class GroupKilled(BaseException):
        pass

    activation_read, activation_write = os.pipe()
    ready_read, ready_write = os.pipe()
    lifeline_read, lifeline_write = os.pipe()
    original_read = os.read
    killed: list[int] = []

    def fail_signal(descriptor: int, signum: int) -> None:
        raise OSError(9, "injected active supervisor descriptor failure")

    def fail_read(descriptor: int, length: int) -> bytes:
        if descriptor == lifeline_read:
            # Readiness proves this is the phase in which C may already run.
            assert original_read(ready_read, 1) == b"1"
            if failure == "lifeline-read":
                raise OSError(5, "injected active lifeline read failure")
            return b""
        return original_read(descriptor, length)

    def kill_group() -> None:
        killed.append(os.getpgrp())
        raise GroupKilled

    monkeypatch.setattr(anchor_module, "_prctl", lambda option, argument: None)
    monkeypatch.setattr(anchor_module, "_read_byte", lambda descriptor, expected_parent: b"1")
    monkeypatch.setattr(signal, "signal", lambda signum, handler: signal.SIG_DFL)
    monkeypatch.setattr(signal, "pidfd_send_signal", fail_signal)
    monkeypatch.setattr(os, "read", fail_read)
    monkeypatch.setattr(anchor_module, "_kill_anchored_group", kill_group)
    try:
        with pytest.raises(GroupKilled):
            anchor_module.run(os.getppid(), activation_read, ready_write, lifeline_read, -1)
        assert killed
    finally:
        for descriptor in (activation_read, activation_write, ready_read, ready_write,
                           lifeline_read, lifeline_write):
            try:
                os.close(descriptor)
            except OSError:
                pass


def test_command_supervisor_crash_after_adapter_start_uses_frozen_census(tmp_path: Path) -> None:
    """A separate reaper models init; the bridge must prove its orphan group vanished."""
    pids = tmp_path / "crashed-supervisor-pids"
    package_parent = Path(chat_module.__file__).resolve().parent.parent
    probe = r'''
import ctypes
import os
import pathlib
import signal
import subprocess
import sys
import time

libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(36, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
caller_program = r"""
import pathlib
import sys
sys.path.insert(0, sys.argv[2])
from agentctl import chat
adapter = (
    "import os,pathlib,subprocess,sys,time; "
    "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
    "pathlib.Path(sys.argv[1]).write_text(f'{os.getppid()} {os.getpgrp()} {os.getpid()} {child.pid}'); "
    "time.sleep(60)"
)
censuses = []
original_census = chat._command_group_members
def census(anchor):
    result = original_census(anchor)
    censuses.append(result)
    return result
chat._command_group_members = census
try:
    chat._run_command((sys.executable, "-c", adapter, sys.argv[1]), timeout=10)
except ValueError as exc:
    if "final result" not in str(exc):
        raise
else:
    raise SystemExit("supervisor crash was incorrectly reported as adapter success")
if len(censuses) < 2:
    raise SystemExit("supervisor crash did not take a stable emergency census")
if not all(item.state in ("T", "t", "Z") for item in censuses[-1]):
    raise SystemExit("emergency census included a running group member")
"""
path = pathlib.Path(sys.argv[1])
caller = subprocess.Popen((sys.executable, "-c", caller_program, str(path), sys.argv[2]))
identities = []
try:
    deadline = time.monotonic() + 5
    while not path.exists() and time.monotonic() < deadline:
        if caller.poll() is not None:
            raise SystemExit("caller failed before adapter started")
        time.sleep(0.005)
    identities = [int(value) for value in path.read_text().split()]
    supervisor, anchor, adapter, descendant = identities
    os.kill(supervisor, signal.SIGKILL)
    deadline = time.monotonic() + 10
    while caller.poll() is None and time.monotonic() < deadline:
        # Only the outer test reaper owns these orphaned processes. Reap each
        # after it exits, without stealing the caller or its direct child S.
        for pid in (anchor, adapter, descendant):
            try:
                os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                pass
        time.sleep(0.005)
    if caller.wait(timeout=1) != 0:
        raise SystemExit("caller did not report a proved containment failure")
    remaining = [pid for pid in identities if pathlib.Path(f"/proc/{pid}").exists()]
    if remaining:
        raise SystemExit(f"supervisor crash left identities: {remaining}")
finally:
    if caller.poll() is None:
        caller.kill()
        caller.wait(timeout=2)
    for pid in identities:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        try:
            pid, _ = os.waitpid(-1, os.WNOHANG)
        except ChildProcessError:
            break
        if not pid:
            time.sleep(0.005)
'''
    completed = subprocess.run(
        (sys.executable, "-c", probe, str(pids), str(package_parent)),
        text=True, capture_output=True, timeout=18, check=False,
    )
    assert completed.returncode == 0, completed.stderr


@pytest.mark.parametrize("operation", ["read", "write"])
def test_command_transport_retries_nonblocking_readiness_race(
    monkeypatch: pytest.MonkeyPatch, operation: str,
) -> None:
    blocked = False
    if operation == "read":
        original_read = os.read

        def flaky_read(fd: int, length: int) -> bytes:
            nonlocal blocked
            if not blocked and not os.get_blocking(fd):
                blocked = True
                raise BlockingIOError
            return original_read(fd, length)

        monkeypatch.setattr(os, "read", flaky_read)
    else:
        original_write = os.write

        def flaky_write(fd: int, data: object) -> int:
            nonlocal blocked
            if not blocked:
                blocked = True
                raise BlockingIOError
            return original_write(fd, cast(bytes, data))

        monkeypatch.setattr(os, "write", flaky_write)
    script = "import sys; data = sys.stdin.buffer.read(); sys.stdout.buffer.write(data)"
    result = _run_command((sys.executable, "-c", script), timeout=5, input_text="ready")
    assert blocked and result.returncode == 0 and result.stdout == "ready" and result.stderr == ""


class Pages:
    def __init__(self, *pages: dict[str, object]) -> None:
        self.pages = list(pages)
        self.calls: list[dict[str, object]] = []

    def __call__(self, request: dict[str, object]) -> dict[str, object]:
        self.calls.append(request)
        return self.pages.pop(0)


def _message(identifier: str, created_at: str = "2026-01-01T00:02:00Z") -> dict[str, object]:
    return {"id": _SPACE + "/messages/" + identifier, "text": identifier,
            "sender": "users/owner", "thread": _THREAD, "created_at": created_at}


def _bridge(state: Path, transport: Pages, *, target: Target | None = None) -> Bridge:
    if not (state / "bridge.json").exists():
        target = target or Target(pane_id="w1:p1", expected_agent="codex",
                                  expected_cwd="/work/project", expected_workspace="project")
        Bridge.initialize(state, Config(_SPACE, ("users/owner",), target, "fixture-agent", ack_reaction=None), after=_START)
    return Bridge(state, transport=transport)


def test_pagination_restarts_same_query_then_overlaps_without_duplicate_inputs(tmp_path: Path) -> None:
    pages = Pages(
        {"messages": [_message("one")], "cursor": "page-two"},
        {"messages": [_message("one"), _message("two")], "cursor": None},
        {"messages": [_message("one"), _message("two"), _message("three")], "cursor": None},
    )
    _bridge(tmp_path, pages)._ingest(_read(tmp_path / "bridge.json"))
    assert _read(tmp_path / "bridge.json")["after"] == _START
    restarted = _bridge(tmp_path, pages)
    restarted._ingest(_read(tmp_path / "bridge.json"))
    assert pages.calls[1] == {"action": "poll", "space": _SPACE, "after": _START, "cursor": "page-two"}
    assert _read(tmp_path / "bridge.json")["after"] == "2026-01-01T00:01:00Z"
    restarted._ingest(_read(tmp_path / "bridge.json"))
    assert len(list((tmp_path / "requests").glob("*.json"))) == 3


def test_failed_page_replays_persisted_messages_without_advancing_cursor(tmp_path: Path) -> None:
    pages = Pages({"messages": [_message("one")], "cursor": 42},
                  {"messages": [_message("one"), _message("two")], "cursor": None})
    bridge = _bridge(tmp_path, pages)
    before = _read(tmp_path / "bridge.json")
    with pytest.raises(ValueError, match="cursor"):
        bridge._ingest(_read(tmp_path / "bridge.json"))
    assert _read(tmp_path / "bridge.json") == before
    _bridge(tmp_path, pages)._ingest(_read(tmp_path / "bridge.json"))
    assert pages.calls[0] == pages.calls[1]
    assert len(list((tmp_path / "requests").glob("*.json"))) == 2


def test_wrong_reply_resource_remains_pending_and_keeps_retry_id(tmp_path: Path) -> None:
    pages = Pages({"id": "spaces/other/messages/reply"}, {"id": _SPACE + "/messages/reply"})
    bridge = _bridge(tmp_path, pages)
    message = _message("one")
    key = hashlib.sha256(str(message["id"]).encode()).hexdigest()
    path = tmp_path / "requests" / f"{key}.json"
    _write(path, {"key": key, "queue_id": "source-one", "phase": "awaiting_reply",
                  "message": message, "request_id": _REQUEST_ID, "received_at": _START})
    bridge.migrate_reply_outboxes()
    submit_reply(tmp_path, key, "Done")
    with pytest.raises(ValueError, match="outside"):
        bridge._deliver()
    assert _read(path)["phase"] == "reply_pending"
    _bridge(tmp_path, pages)._deliver()
    assert _read(path)["phase"] == "replied"
    assert pages.calls[0]["request_id"] == pages.calls[1]["request_id"] == _REQUEST_ID


def test_restart_rejects_replacement_session_before_any_chat_access(tmp_path: Path) -> None:
    class ReplacementHarness(HerdrClient):
        def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
            return (Pane("w1:p1", "w1:t1", "w1"),)

        def pane_info(self, pane_id: str) -> AgentPaneInfo:
            return AgentPaneInfo(pane_id, "w1", "/work/project", "codex", "idle", "codex", "replacement")

    pages = Pages()
    target = Target(pane_id="w1:p1", session_agent="codex", session_value="original",
                    expected_agent="codex", expected_cwd="/work/project", expected_workspace="project")
    _bridge(tmp_path, pages, target=target)
    restarted = Bridge(tmp_path, ReplacementHarness(), pages)
    with pytest.raises(AgentDeliveryError, match="exactly one live pane"):
        restarted.tick()
    assert pages.calls == []


class NamedHarness(HerdrClient):
    def __init__(self) -> None:
        self.named_pane = "w1:p1"
        self.probes = 0
        self.replace_on_probe: int | None = None
        self.prompts: list[str] = []

    def agent_pane(self, name: str) -> str:
        assert name == "coordinator"
        return self.named_pane

    def pane_info(self, pane_id: str) -> AgentPaneInfo:
        self.probes += 1
        if self.probes == self.replace_on_probe:
            self.named_pane = "w1:p2"
        return AgentPaneInfo(pane_id, "w1", "/work/project", "codex", "idle", None, None)

    def workspace_label(self, workspace_id: str) -> str:
        return "project"

    def prompt_agent(
        self, pane_id: str, command: str, *, terminal: GuardedInput | None = None,
    ) -> None:
        self.prompts.append(command)

    def wait_agent_status(self, pane_id: str, status: str, timeout_ms: int) -> None:
        assert status == "working"


def _named_bridge(state: Path, client: NamedHarness, transport: Pages) -> Bridge:
    target = Target(pane_id="w1:p1", expected_agent="codex",
                    expected_cwd="/work/project", expected_workspace="project")
    Bridge.initialize(state, Config(_SPACE, ("users/owner",), target, "fixture-agent",
                                    agent_name="coordinator", ack_reaction=None), after=_START)
    return Bridge(state, client, transport)


def test_named_binding_survives_restart_and_blocks_changed_owner_before_chat(tmp_path: Path) -> None:
    client, pages = NamedHarness(), Pages()
    _named_bridge(tmp_path, client, pages)
    client.named_pane = "w1:p2"
    restarted = Bridge(tmp_path, client, pages)
    assert restarted.config.agent_name == "coordinator"
    with pytest.raises(HerdrUnavailable, match="no longer owns"):
        restarted.tick()
    assert client.prompts == []
    assert pages.calls == []


def test_named_binding_checked_again_after_poll_before_delivery(tmp_path: Path) -> None:
    client = NamedHarness()

    class ChangedDuringPoll(Pages):
        def __call__(self, request: dict[str, object]) -> dict[str, object]:
            client.named_pane = "w1:p2"
            return super().__call__(request)

    pages = ChangedDuringPoll({"messages": [_message("one")], "cursor": None})
    bridge = _named_bridge(tmp_path, client, pages)
    bridge.tick()
    record = _read(next((tmp_path / "requests").glob("*.json")))
    assert record["phase"] == "queued"
    # The queue preserves known-unsubmitted work; the next tick rejects its
    # stale named target before even polling again.
    with pytest.raises(HerdrUnavailable, match="no longer owns"):
        bridge.tick()
    assert client.prompts == []
    assert len(pages.calls) == 1


def test_named_binding_checked_immediately_before_terminal_submission(tmp_path: Path) -> None:
    client = NamedHarness()
    # Tick, queue-lock resolution, then confirmation under that lock. The last
    # readiness snapshot still describes the old occupant; run must recheck.
    client.replace_on_probe = 3
    pages = Pages({"messages": [_message("one")], "cursor": None})
    bridge = _named_bridge(tmp_path, client, pages)
    bridge.tick()
    assert client.prompts == []
    record = _read(next((tmp_path / "requests").glob("*.json")))
    assert record["phase"] == "delivery_uncertain"


def test_named_coordinator_delivers_when_binding_is_unchanged(tmp_path: Path) -> None:
    client = NamedHarness()
    pages = Pages({"messages": [_message("one")], "cursor": None})
    bridge = _named_bridge(tmp_path, client, pages)
    bridge.tick()
    assert len(client.prompts) == 1
    assert client.probes >= 5


def test_run_loop_retries_malformed_transport_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    calls = 0
    now = 0.0

    class RetryBridge(Bridge):
        def __init__(self, state: Path) -> None:
            self.state = state
            self.config = Config(_SPACE, ("users/owner",), Target(), "test-agent", reply_mode="file")

        def tick(self) -> dict[str, object]:
            nonlocal calls
            calls += 1
            if calls == 1:
                raise TypeError("malformed adapter message")
            raise KeyboardInterrupt

        def output_requests(self, *, retry_failed: bool = False) -> dict[str, str]:
            return {}

        def output_markers(self, *, retry_failed: bool = False) -> tuple[str, ...]:
            return ()

        def _prime_prompt_cache(self) -> None:
            return None

        def migrate_reply_outboxes(self) -> None:
            pass

        def validate_continuous_output(self) -> None:
            pass

    def skip_sleep(seconds: float) -> None:
        nonlocal now
        now += seconds

    monkeypatch.setattr(chat_module, "Bridge", RetryBridge)
    monkeypatch.setattr(time, "sleep", skip_sleep)
    monkeypatch.setattr(time, "monotonic", lambda: now)
    assert chat_module.run_cli(["run", "--state", str(tmp_path)]) == 130
    assert calls == 2
    assert "malformed adapter message" in capsys.readouterr().err


@pytest.mark.parametrize(("interval_arguments", "failures", "expected_delays"), [
    (("--interval", "10"), 4, [20, 40, 60, 60, 10]),
    ((), 2, [7200, 14400, 3600]),
])
def test_run_loop_backs_off_failures_and_recovers_configured_interval(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    interval_arguments: tuple[str, ...], failures: int, expected_delays: list[float],
) -> None:
    calls = 0
    now = 0.0
    delays: list[float] = []
    tick_times: list[float] = []

    class RecoveringBridge(Bridge):
        def __init__(self, state: Path) -> None:
            self.state = state
            self.config = Config(_SPACE, ("users/owner",), Target(), "test-agent", reply_mode="file")

        def tick(self) -> dict[str, object]:
            nonlocal calls
            tick_times.append(now)
            calls += 1
            if calls <= failures:
                raise ValueError("temporary quota failure")
            if calls == failures + 1:
                return {}
            raise KeyboardInterrupt

        def output_requests(self, *, retry_failed: bool = False) -> dict[str, str]:
            return {}

        def output_markers(self, *, retry_failed: bool = False) -> tuple[str, ...]:
            return ()

        def _prime_prompt_cache(self) -> None:
            return None

        def migrate_reply_outboxes(self) -> None:
            pass

        def validate_continuous_output(self) -> None:
            pass

    def sleep(seconds: float) -> None:
        nonlocal now
        delays.append(seconds)
        now += seconds

    monkeypatch.setattr(chat_module, "Bridge", RecoveringBridge)
    monkeypatch.setattr(time, "sleep", sleep)
    monkeypatch.setattr(time, "monotonic", lambda: now)
    assert chat_module.run_cli([
        "run", "--state", str(tmp_path), *interval_arguments,
    ]) == 130
    expected_ticks = [0.0]
    for delay in expected_delays:
        expected_ticks.append(expected_ticks[-1] + delay)
    assert tick_times == expected_ticks
    assert delays == expected_delays


def test_run_loop_long_interval_failures_back_off_toward_one_day(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls = 0
    now = 0.0
    delays: list[float] = []
    tick_times: list[float] = []

    class RecoveringBridge(Bridge):
        def __init__(self, state: Path) -> None:
            self.state = state
            self.config = Config(_SPACE, ("users/owner",), Target(), "test-agent", reply_mode="file")

        def tick(self) -> dict[str, object]:
            nonlocal calls
            tick_times.append(now)
            calls += 1
            if calls <= 2:
                raise ValueError("temporary quota failure")
            if calls == 3:
                return {}
            raise KeyboardInterrupt

        def output_requests(self, *, retry_failed: bool = False) -> dict[str, str]:
            return {}

        def output_markers(self, *, retry_failed: bool = False) -> tuple[str, ...]:
            return ()

        def _prime_prompt_cache(self) -> None:
            return None

        def migrate_reply_outboxes(self) -> None:
            pass

        def validate_continuous_output(self) -> None:
            pass

    def sleep(seconds: float) -> None:
        nonlocal now
        delays.append(seconds)
        now += seconds

    monkeypatch.setattr(chat_module, "Bridge", RecoveringBridge)
    monkeypatch.setattr(time, "sleep", sleep)
    monkeypatch.setattr(time, "monotonic", lambda: now)
    assert chat_module.run_cli([
        "run", "--state", str(tmp_path), "--interval", "3600",
    ]) == 130
    assert tick_times == [0, 7200, 21600, 25200]
    assert delays == [7200, 14400, 3600]


def test_run_accepts_one_day_poll_interval_and_rejects_larger(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    intervals: list[float] = []
    monkeypatch.setattr(chat_module, "Bridge", lambda state: object())
    monkeypatch.setattr(chat_module, "_run_bridge",
                        lambda bridge, interval, prog, **kwargs: intervals.append(interval))

    assert chat_module.run_cli([
        "run", "--state", str(tmp_path), "--interval", "86400",
    ]) == 0
    assert intervals == [86400]
    assert chat_module.run_cli([
        "run", "--state", str(tmp_path), "--interval", "86400.1",
    ]) == 1
    assert "between 0.1 and 86400 seconds" in capsys.readouterr().err


def test_run_cli_reports_sigterm_as_clean_service_shutdown(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(chat_module, "Bridge", lambda state: object())

    def terminated(*args: object, **kwargs: object) -> None:
        raise chat_module._ServiceTerminated

    monkeypatch.setattr(chat_module, "_run_bridge", terminated)
    assert chat_module.run_cli(["run", "--state", str(tmp_path)]) == 0


def test_run_validates_observer_write_interval(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    intervals: list[float] = []
    monkeypatch.setattr(chat_module, "Bridge", lambda state: object())
    monkeypatch.setattr(chat_module, "_run_bridge",
        lambda bridge, interval, prog, **kwargs: intervals.append(
            float(kwargs["observer_write_interval"])))

    for value in (60.0, 3600.0):
        assert chat_module.run_cli([
            "run", "--state", str(tmp_path),
            "--observer-write-interval", str(value),
        ]) == 0
    assert intervals == [60, 3600]
    for invalid_value in (59.9, 3600.1):
        assert chat_module.run_cli([
            "run", "--state", str(tmp_path),
            "--observer-write-interval", str(invalid_value),
        ]) == 1
    assert capsys.readouterr().err.count("between 60 and 3600 seconds") == 2

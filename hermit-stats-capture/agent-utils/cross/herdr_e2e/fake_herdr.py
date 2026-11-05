"""A stand-in for the ``herdr`` command line, for the herdr-run end-to-end differential.

The differential installs a copy of this file as ``<account home>/.local/bin/fake_herdr.py``,
and beside it, as ``<account home>/.local/bin/herdr`` -- one of the fixed places both editions
look for Herdr -- a ``/bin/sh`` launcher (``LAUNCHER`` in ``cross/herdr_differential.py``). It
makes that directory the account home with the preloaded shim beside this file, so both editions
talk to the fake exactly as they talk to a real Herdr: through the production client, by absolute
path, with ``PATH`` removed. Nothing here is ever started from a real account home, and it never
starts a server.

The launcher reads what the call was handed before any interpreter can change it -- its ignored
signals (a Python interpreter ignores SIGPIPE and SIGXFSZ for itself as it starts, and a launcher in
front of one may ignore more) and its environment, entry by entry, as the kernel holds it -- and
passes both on in ``HERDR_E2E_HANDED_SIGNALS`` (the ``SigIgn`` mask) and
``HERDR_E2E_HANDED_ENVIRON`` (the raw entries, NUL-separated, in hexadecimal).

State lives in ``<account home>/herdr-state/``:

* ``state.json``: workspaces, tabs, and panes, each pane with the PID and start time of a
  ``sleep`` process that stands in for its shell, so ``/proc`` evidence (start ticks, liveness)
  is real;
* ``calls.jsonl``: one JSON object per call: ``argv``, the arguments; ``environ``, the entries it
  was handed, in order, exactly (bytes that are not UTF-8 as surrogate escapes); and
  ``ignored_signals``, the signals it was handed ignored;
* ``behaviour.json``, read on every call, which a case writes to make this Herdr misbehave:

  ``fail``       {"<two words>": "message"}: exit 3 with ``message`` on stderr
  ``raw``        {"<two words>": "text"}: exit 0 writing ``text`` instead of the real reply
  ``busy``       true, or a list of pane IDs, whose foreground is not their shell
  ``fg_name``    the foreground process name ``process-info`` reports (default ``bash``)
  ``pane_text``  what ``pane read`` prints (default ``user@host:~$ `` and a newline)
  ``pane_path``  ``PATH`` for commands ``pane run`` executes (default ``/usr/bin:/bin``)

``<two words>`` is the subcommand and its verb, ``workspace list`` or ``pane run``; ``status``
stands alone. Every process this fake starts is recorded in ``state.json`` under ``processes``, as
``[pid, start ticks]``, so the harness can end it when the case is over -- and only it: a PID whose
start time no longer matches has been given to another process, which nothing here touches.
"""

from __future__ import annotations

import fcntl
import json
import os
import shutil
import subprocess
import sys

ACCOUNT_HOME = os.path.dirname(os.path.dirname(os.path.dirname(os.path.realpath(__file__))))
STATE_DIRECTORY = os.path.join(ACCOUNT_HOME, "herdr-state")
STATE = os.path.join(STATE_DIRECTORY, "state.json")
CALLS = os.path.join(STATE_DIRECTORY, "calls.jsonl")
BEHAVIOUR = os.path.join(STATE_DIRECTORY, "behaviour.json")
QUIET_PATH = "/usr/bin:/bin"
#: A busy pane's foreground process group is its shell's PID plus this.
FOREGROUND_OFFSET = 2_000_000_000


def _load(path: str, default: object) -> object:
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except FileNotFoundError:
        return default


def _save(state: dict[str, object]) -> None:
    temporary = STATE + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=1, sort_keys=True)
    os.replace(temporary, STATE)


def start_ticks(pid: int) -> int | None:
    """When ``pid`` started, in clock ticks since boot (field 22 of ``/proc/<pid>/stat``)."""
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8", errors="surrogateescape") as handle:
            fields = handle.read().rsplit(") ", 1)[-1].split()
    except OSError:
        return None
    try:
        return int(fields[19])
    except (IndexError, ValueError):
        return None


def _spawn(
    state: dict[str, object], argv: list[str], environment: dict[str, str]
) -> tuple[int, int | None]:
    """Start ``argv`` in a session of its own; record it, with its start time, in ``state``."""
    process = subprocess.Popen(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
        close_fds=True,
        env=environment,
        cwd="/",
    )
    # Not yet reaped, whether it is still running or has already exited, so this is its own entry.
    started = start_ticks(process.pid)
    processes = state.setdefault("processes", [])
    assert isinstance(processes, list)
    processes.append([process.pid, started])
    return process.pid, started


def _kill_if_same(pid: int, started: object) -> None:
    """SIGKILL ``pid`` only while it is still the process that started at ``started``."""
    if started is not None and start_ticks(pid) == started:
        try:
            os.kill(pid, 9)
        except OSError:
            pass


def _shell_stand_in(state: dict[str, object]) -> tuple[int, int | None]:
    sleep = shutil.which("sleep", path=QUIET_PATH) or "/bin/sleep"
    return _spawn(state, [sleep, "86400"], {"PATH": QUIET_PATH})


def _next(state: dict[str, object], kind: str) -> int:
    counters = state.setdefault("counters", {})
    assert isinstance(counters, dict)
    counters[kind] = int(counters.get(kind, 0)) + 1
    return int(counters[kind])


def _option(args: list[str], name: str) -> str | None:
    return args[args.index(name) + 1] if name in args and args.index(name) + 1 < len(args) else None


class Reply(Exception):
    def __init__(self, status: int, stdout: str = "", stderr: str = "") -> None:
        super().__init__(status)
        self.status = status
        self.stdout = stdout
        self.stderr = stderr


def _result(document: object) -> Reply:
    return Reply(0, json.dumps({"result": document}) + "\n")


def _new_pane(state: dict[str, object], workspace: str, tab: str) -> str:
    pane = f"{workspace}:p{_next(state, 'pane')}"
    panes = state.setdefault("panes", [])
    shells = state.setdefault("shells", {})
    assert isinstance(panes, list) and isinstance(shells, dict)
    shell, shell_start = _shell_stand_in(state)
    panes.append(
        {"id": pane, "tab": tab, "workspace": workspace, "shell": shell, "shell_start": shell_start}
    )
    # Kept after the pane closes, so the harness can still name the PID wherever it appears.
    shells[pane] = shell
    return pane


def handle(args: list[str], state: dict[str, object], behaviour: dict[str, object]) -> Reply:
    workspaces = state.setdefault("workspaces", [])
    tabs = state.setdefault("tabs", [])
    panes = state.setdefault("panes", [])
    assert isinstance(workspaces, list) and isinstance(tabs, list) and isinstance(panes, list)
    words = " ".join(args[:1]) if args[:1] == ["status"] else " ".join(args[:2])
    failures = behaviour.get("fail", {})
    raw = behaviour.get("raw", {})
    assert isinstance(failures, dict) and isinstance(raw, dict)
    if words in failures:
        return Reply(3, "", f"{failures[words]}\n")
    if words in raw:
        return Reply(0, str(raw[words]))
    if words == "status":
        return Reply(0, json.dumps({"server": {"running": True}}) + "\n")
    if words == "workspace list":
        return _result(
            {"workspaces": [{"workspace_id": w["id"], "label": w["label"]} for w in workspaces]}
        )
    if words == "workspace create":
        workspace = f"w{_next(state, 'workspace')}"
        tab = f"{workspace}:t{_next(state, 'tab')}"
        workspaces.append({"id": workspace, "label": _option(args, "--label")})
        tabs.append({"id": tab, "workspace": workspace, "label": "1"})
        pane = _new_pane(state, workspace, tab)
        return _result(
            {
                "workspace": {"workspace_id": workspace},
                "tab": {"tab_id": tab},
                "root_pane": {"pane_id": pane},
            }
        )
    if words == "tab list":
        listed = _option(args, "--workspace")
        return _result(
            {
                "tabs": [
                    {"tab_id": t["id"], "label": t["label"]}
                    for t in tabs
                    if t["workspace"] == listed
                ]
            }
        )
    if words == "tab create":
        owner = _option(args, "--workspace") or ""
        created = f"{owner}:t{_next(state, 'tab')}"
        tabs.append({"id": created, "workspace": owner, "label": _option(args, "--label")})
        _new_pane(state, owner, created)
        return _result({"tab": {"tab_id": created}})
    if words == "tab rename":
        for entry in tabs:
            if entry["id"] == args[2]:
                entry["label"] = args[3]
        return _result({})
    if words == "tab close":
        for entry in panes:
            if entry["tab"] == args[2]:
                _kill_if_same(int(str(entry["shell"])), entry.get("shell_start"))
        state["tabs"] = [entry for entry in tabs if entry["id"] != args[2]]
        state["panes"] = [entry for entry in panes if entry["tab"] != args[2]]
        return Reply(0)
    if words == "pane list":
        listed = _option(args, "--workspace")
        return _result(
            {
                "panes": [
                    {"pane_id": p["id"], "tab_id": p["tab"], "workspace_id": p["workspace"]}
                    for p in panes
                    if listed is None or p["workspace"] == listed
                ]
            }
        )
    if words == "pane get":
        if any(entry["id"] == args[2] for entry in panes):
            return _result({"pane": {"pane_id": args[2]}})
        return Reply(1, "", f"no such pane: {args[2]}\n")
    if words == "pane process-info":
        wanted = _option(args, "--pane")
        found = next((p for p in panes if p["id"] == wanted), None)
        if found is None:
            return Reply(1, "", f"no such pane: {wanted}\n")
        shell = int(str(found["shell"]))
        busy = behaviour.get("busy", False)
        name = str(behaviour.get("fg_name", "bash"))
        if busy is True or (isinstance(busy, list) and wanted in busy):
            # Far from any real PID, so it can never be mistaken for another pane's shell.
            foreground = shell + FOREGROUND_OFFSET
            processes = [{"pid": foreground, "name": "vim", "cmdline": "vim notes.txt"}]
        else:
            foreground = shell
            processes = [{"pid": shell, "name": name, "cmdline": name}]
        return _result(
            {
                "process_info": {
                    "pane_id": wanted,
                    "shell_pid": shell,
                    "foreground_process_group_id": foreground,
                    "foreground_processes": processes,
                }
            }
        )
    if words == "pane read":
        return Reply(0, str(behaviour.get("pane_text", "user@host:~$ \n")))
    if words == "pane run":
        environment = {
            "PATH": str(behaviour.get("pane_path", QUIET_PATH)),
            "HOME": ACCOUNT_HOME,
            "LANG": "C.UTF-8",
            "TZ": "UTC",
        }
        _spawn(state, ["/bin/bash", "-c", args[3]], environment)
        return Reply(0)
    return Reply(2, "", f"fake herdr: unknown command {args!r}\n")


def _handed() -> dict[str, object]:
    """What the launcher read before this interpreter started: the environment and ignored signals.

    Taken out of this process's own environment, so nothing this fake starts sees them.
    """
    encoded = os.environ.pop("HERDR_E2E_HANDED_ENVIRON", None)
    mask_text = os.environ.pop("HERDR_E2E_HANDED_SIGNALS", None)
    raw = bytes.fromhex(encoded) if encoded is not None else b""
    entries = raw[:-1].split(b"\0") if raw.endswith(b"\0") else ([raw] if raw else [])
    mask = int(mask_text.strip() or "0", 16) if mask_text is not None else 0
    return {
        "environ": [entry.decode("utf-8", errors="surrogateescape") for entry in entries],
        "ignored_signals": [number for number in range(1, 65) if mask & (1 << (number - 1))],
    }


def main() -> int:
    args = sys.argv[1:]
    handed = _handed()
    os.makedirs(STATE_DIRECTORY, exist_ok=True)
    lock = os.open(STATE + ".lock", os.O_CREAT | os.O_RDWR, 0o600)
    fcntl.flock(lock, fcntl.LOCK_EX)
    with open(CALLS, "a", encoding="utf-8") as log:
        log.write(json.dumps({"argv": args, **handed}) + "\n")
    state = _load(STATE, {})
    behaviour = _load(BEHAVIOUR, {})
    assert isinstance(state, dict) and isinstance(behaviour, dict)
    try:
        reply = handle(args, state, behaviour)
    except Reply as error:  # pragma: no cover - handle returns, never raises a Reply
        reply = error
    _save(state)
    fcntl.flock(lock, fcntl.LOCK_UN)
    # A reply may hold bytes that are not UTF-8 (carried as surrogate escapes); write them as such.
    sys.stdout.buffer.write(reply.stdout.encode("utf-8", errors="surrogateescape"))
    sys.stderr.buffer.write(reply.stderr.encode("utf-8", errors="surrogateescape"))
    return reply.status


if __name__ == "__main__":
    raise SystemExit(main())

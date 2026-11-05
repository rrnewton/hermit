#!/usr/bin/env python3
"""Black-box Python/Rust differential for :command:`herdr-agent`.

Each implementation talks to an isolated executable protocol fixture through its production
client path. The fixture models pane identity, stable sessions, native readiness events, atomic
submission, transcript reads, busy targets, and ambiguous transport failure. The harness also
compares durable queue transitions and crash-sensitive machine outcomes.
"""

from __future__ import annotations

import json
import hashlib
import os
import shlex
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import cast

REPO_ROOT = Path(__file__).resolve().parent.parent
TIMEOUT_SECONDS = 30
_MESSAGE_ID = re.compile(r"\b\d{20,}-\d+\b")


@dataclass(frozen=True)
class Outcome:
    """One normalized command result."""

    returncode: int
    stdout: str
    stderr: str


def _stop_recovery_arguments(outcome: Outcome) -> tuple[str, ...] | None:
    """Parse one copyable command only when a refusal also explains its reason."""
    prefix = "Recovery command: "
    lines = outcome.stderr.splitlines()
    commands = [line[len(prefix):] for line in lines if line.startswith(prefix)]
    explained = any(
        line.startswith(("agentctl: ", "agentctl stop: ", "herdr-agent: ", "error: "))
        and bool(line.partition(":")[2].strip())
        for line in lines
    )
    if len(commands) != 1 or not explained:
        return None
    try:
        arguments = tuple(shlex.split(commands[0]))
    except ValueError:
        return None
    return arguments if arguments and arguments[0] == "agentctl" else None


@dataclass
class Report:
    """Accumulated cross-edition assertions."""

    checks: int = 0
    failures: list[str] = field(default_factory=list)

    def require(self, label: str, condition: bool, detail: str) -> None:
        self.checks += 1
        if not condition:
            self.failures.append(f"{label}: {detail}")

    def exact(self, label: str, python: Outcome, rust: Outcome, expected_rc: int) -> None:
        self.require(
            label,
            python == rust and python.returncode == expected_rc,
            f"expected rc {expected_rc}; python={python!r} rust={rust!r}",
        )


@dataclass(frozen=True)
class PairCase:
    """Equivalent isolated state roots and protocol fixtures."""

    python_root: Path
    rust_root: Path


_FAKE_HERDR = r'''import json
import os
import shlex
import signal
import subprocess
import sys
import time

root = os.path.dirname(os.path.realpath(__file__))
state_path = os.path.join(root, "state.json")
with open(state_path, encoding="utf-8") as handle:
    state = json.load(handle)
args = sys.argv[1:]
state.setdefault("calls", []).append(args)
current_pane = state.get("pane_id", "w1:p1")
current_tab = state.get("tab_id", "w1:t1")
current_workspace = state.get("workspace_id", "w1")
current_terminal = state.get("terminal_id", "term-1")
expected_terminal = None
if (len(args) > 3 and args[2] == "--expect-terminal"
        and args[:2] in (["pane", "send-text"], ["pane", "send-keys"], ["agent", "prompt"])):
    expected_terminal = args[3]
    args = args[:2] + args[4:]

if args == ["goal-rpc"]:
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        result = {}
        if request.get("method") == "thread/goal/get":
            goals = [text[6:] for text in state.get("submitted", []) if text.startswith("/goal ")]
            result = {"goal": None if not goals else {
                "threadId": request["params"]["threadId"], "objective":goals[-1], "status":"active",
                "tokensUsed":1, "timeUsedSeconds":1, "createdAt":1, "updatedAt":1, "tokenBudget":None,
            }}
        print(json.dumps({"id":request["id"], "result":result}), flush=True)
    raise SystemExit(0)

def save():
    temporary = state_path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, sort_keys=True)
    os.replace(temporary, state_path)

def envelope(result):
    print(json.dumps({"result": result}, sort_keys=True))

def outside_case(program):
    # A bare name would be looked up on PATH; a path must stay inside this case directory both
    # with links followed before `..` (the kernel) and with `..` removed first.
    if os.sep not in program:
        return True
    joined = os.path.join(os.getcwd(), program)
    readings = (os.path.realpath(joined), os.path.realpath(os.path.normpath(joined)))
    return any(os.path.commonpath((reading, root)) != root for reading in readings)

def verified_composer():
    # Claude and Codex panes render a composer that clients stage text into and submit from.
    return not state.get("custom_harness") and state.get("harness", "codex") in ("claude", "codex")

def after_write(text):
    # Injected transport behavior once a prompt has reached the agent.
    if state.get("run_mode") == "gate":
        save()
        with open(os.path.join(root, "run-entered"), "w", encoding="utf-8") as handle:
            handle.write(text)
        deadline = time.monotonic() + 20
        while not os.path.exists(os.path.join(root, "run-release")):
            if time.monotonic() >= deadline:
                print("gate timed out", file=sys.stderr)
                raise SystemExit(1)
            time.sleep(0.01)
    if state.get("run_mode") == "fail":
        print("transport lost after write", file=sys.stderr)
        save()
        raise SystemExit(1)

def composer_screen():
    dim = lambda text: "\x1b[2m" + text + "\x1b[22m"
    draft = state.get("composer_draft", "")
    busy = state.get("status") == "working" or state.get("composer_busy")
    if state.get("harness", "codex") == "claude":
        rule = "\u2500" * 40
        lines = ["\u2022 earlier output"]
        for text in state.get("submitted", []):
            lines += ["\u276f " + text, "\u25cf accepted"]
        lines.append(rule)
        rows = draft.split("\n") if draft else [""]
        lines.append("\u276f " + rows[0] if draft else "\u276f\xa0")
        lines += ["  " + row for row in rows[1:]]
        lines += [rule, "  auto mode on" + (" \u00b7 esc to interrupt" if busy else "")]
    else:
        lines = ["\u2022 earlier output"]
        for text in state.get("submitted", []):
            lines += ["\u203a " + text, "\u2022 accepted"]
        if busy:
            lines.append(state.get("codex_activity", "Working (esc to interrupt)"))
        if draft:
            rows = draft.split("\n")
            lines.append("\u203a " + rows[0])
            lines += ["  " + row for row in rows[1:]]
        else:
            lines.append("\u203a " + dim("Ask Codex to do anything"))
        lines += ["", "  tab to queue message" if busy and draft else
                  state.get("codex_footer", "  model default \u00b7 project")]
    return "\n".join(lines) + "\n"

def revive_fixture():
    # Recovery needs an old idle pane and a distinct newly allocated pane. The kernel
    # processes are children of the test driver, which kills and reaps only those children.
    panes = state.setdefault("revive_panes", {})

    def missing(kind, target):
        print(json.dumps({"error": {"code": kind + "_not_found",
              "message": "missing " + target}}), file=sys.stderr)
        save()
        raise SystemExit(1)

    def pane_at(target):
        pane = panes.get(target)
        if pane is None or pane.get("closed"):
            missing("pane", target)
        return pane

    def presentation(pane_id, pane):
        return {"pane_id":pane_id, "tab_id":pane["tab_id"],
                "workspace_id":pane["workspace_id"]}

    def agent_identity(pane_id, pane):
        return {"name":pane["name"], "pane_id":pane_id, "tab_id":pane["tab_id"],
                "terminal_id":pane["terminal_id"]}

    live = {pane_id:pane for pane_id, pane in panes.items() if not pane.get("closed")}
    if args[:2] == ["pane", "list"]:
        envelope({"panes":[presentation(pane_id, pane) for pane_id, pane in live.items()]})
    elif args[:2] == ["pane", "get"]:
        pane = pane_at(args[2])
        idle = pane.get("empty_shell", True)
        envelope({"pane": {**presentation(args[2], pane), "cwd":root,
            "terminal_id":pane["terminal_id"], "agent":None if idle else pane["harness"],
            "agent_status":"unknown" if idle else pane.get("status", "idle"),
            "agent_session":None if idle else {
                "agent":pane["harness"], "value":pane["session_value"],
            },
        }})
    elif args[:2] == ["pane", "process-info"]:
        pane_id = args[args.index("--pane") + 1]
        pane = pane_at(pane_id)
        shell_pid = state["fixture_shell_pid"]
        if pane.get("empty_shell", True):
            pid, executable = shell_pid, state["fixture_shell_executable"]
        else:
            pid, executable = pane["harness_pid"], "/usr/local/bin/" + pane["harness"]
        envelope({"process_info": {"pane_id":pane_id, "shell_pid":shell_pid,
            "foreground_process_group_id":pid,
            "foreground_processes":[{"pid":pid, "name":os.path.basename(executable),
                "cmdline":executable, "argv":[executable], "executable":executable}],
        }})
    elif args[:2] == ["tab", "create"]:
        number = state.get("revive_tabs_created", 0) + 1
        state["revive_tabs_created"] = number
        pane_id, tab_id = "w1:p" + str(number), "w1:t" + str(number)
        panes[pane_id] = {
            "tab_id":tab_id, "workspace_id":args[args.index("--workspace") + 1],
            "terminal_id":"term-" + str(number), "empty_shell":True,
            "label":args[args.index("--label") + 1],
        }
        envelope({"tab":{"tab_id":tab_id}, "root_pane":presentation(pane_id, panes[pane_id])})
    elif args[:2] == ["agent", "start"]:
        pane = pane_at(args[args.index("--pane") + 1])
        index = state.get("revive_launch_count", 0)
        pids = state["revive_harness_pids"]
        if index >= len(pids):
            raise SystemExit("fixture refuses an unplanned extra harness launch")
        launch = args[args.index("--") + 1:]
        session = "session-1"
        for selector in ("--session-id", "--resume", "resume"):
            if selector in launch:
                session = launch[launch.index(selector) + 1]
                break
        pane.update({"name":args[2], "harness":args[args.index("--kind") + 1],
            "harness_pid":pids[index], "session_value":session, "empty_shell":False,
            "status":"idle", "launch_arguments":launch,
        })
        state["revive_launch_count"] = index + 1
        state.setdefault("revive_launch_arguments", []).append(launch)
    elif args[:2] == ["agent", "get"]:
        matches = [(pane_id, pane) for pane_id, pane in live.items()
                   if not pane.get("empty_shell", True) and pane.get("name") == args[2]]
        if len(matches) != 1:
            missing("agent", args[2])
        envelope({"agent":agent_identity(*matches[0])})
    elif args[:2] == ["agent", "list"]:
        envelope({"agents":[agent_identity(pane_id, pane) for pane_id, pane in live.items()
                            if not pane.get("empty_shell", True) and pane.get("name")]})
    elif args[:2] in (["tab", "get"], ["tab", "list"]):
        tabs = [{"tab_id":pane["tab_id"], "workspace_id":pane["workspace_id"],
                 "label":pane["label"], "pane_count":1} for pane in live.values()]
        if args[1] == "list":
            envelope({"tabs":tabs})
        else:
            matches = [tab for tab in tabs if tab["tab_id"] == args[2]]
            if len(matches) != 1:
                missing("tab", args[2])
            envelope({"tab":matches[0]})
    elif args[:2] == ["workspace", "get"]:
        if args[2] != "w1":
            missing("workspace", args[2])
        envelope({"workspace":{"workspace_id":"w1", "label":"project"}})
    elif args[:2] == ["workspace", "list"]:
        envelope({"workspaces":[{"workspace_id":"w1", "label":"project"}]})
    elif args[:2] == ["pane", "read"]:
        pane = pane_at(args[2])
        if args[2] == "w1:p1" and state.pop("revive_fail_old_read_once", False):
            print("injected old output read interruption", file=sys.stderr)
            save()
            raise SystemExit(1)
        source = args[args.index("--source") + 1]
        if source == "visible" and not pane.get("empty_shell", True):
            state["harness"], state["status"] = pane["harness"], pane.get("status", "idle")
            sys.stdout.write(composer_screen())
        else:
            sys.stdout.write("preserved old terminal output\n")
    elif args[:2] == ["pane", "close"]:
        pane = pane_at(args[2])
        if args[2] == "w1:p1" and state.pop("revive_fail_old_close_once", False):
            print("injected stale pane close interruption", file=sys.stderr)
            save()
            raise SystemExit(1)
        pane["closed"] = True
        state.setdefault("closed_panes", []).append(args[2])
    elif args[:2] == ["tab", "close"]:
        matches = [pane_id for pane_id, pane in live.items() if pane["tab_id"] == args[2]]
        if len(matches) != 1:
            missing("tab", args[2])
        pane_at(matches[0])["closed"] = True
        state.setdefault("closed_tabs", []).append(args[2])
    elif args[:2] in (["agent", "focus"], ["tab", "focus"]):
        state["focused"] = args[2]
    elif args[:2] == ["tab", "rename"]:
        matches = [pane for pane in live.values() if pane["tab_id"] == args[2]]
        if len(matches) != 1:
            missing("tab", args[2])
        matches[0]["label"] = " ".join(args[3:])
    elif args[:2] == ["agent", "rename"]:
        pane_at(args[2])["name"] = args[3]
    elif args[:2] == ["status", "server"]:
        print("status: running\nversion: 0.8.0\nprotocol: 20\ncapabilities: input-expect")
    elif args[:2] == ["agent", "wait"]:
        envelope({"agent":{"pane_id":args[2], "agent_status":"working"}})
    elif args[:2] == ["pane", "report-agent-session"]:
        pane_at(args[2])["session_value"] = args[args.index("--agent-session-id") + 1]
    elif args[:2] in (["pane", "send-text"], ["pane", "send-keys"], ["agent", "prompt"]):
        # Every input effect is observable, including a replay that never submits a prompt.
        state.setdefault("revive_input_calls", []).append(args)
        if args[:2] == ["agent", "prompt"]:
            state.setdefault("submitted", []).append(args[3])
    else:
        print("unsupported recovery fixture call: " + repr(args), file=sys.stderr)
        save()
        raise SystemExit(2)
    save()

if state.get("revive_mode"):
    revive_fixture()
    raise SystemExit(0)

if expected_terminal is not None and (
        not state.get("input_expect") or expected_terminal != current_terminal):
    print(json.dumps({"error": {"code": "expectation_failed", "message":
        f"pane {args[2]} holds terminal {current_terminal}, expected {expected_terminal}"}}),
        file=sys.stderr)
    save()
    raise SystemExit(1)
if args[:2] == ["pane", "list"]:
    if state.get("offline"):
        print("server unavailable", file=sys.stderr)
        save()
        raise SystemExit(1)
    panes = [] if state.get("closed") or state.get("missing_pane") else [{
        "pane_id": current_pane, "tab_id": current_tab,
        "workspace_id": current_workspace,
    }]
    if state.get("duplicate_recorded_pane"):
        panes.append({"pane_id":"w1:p1", "tab_id":"w1:duplicate", "workspace_id":"w1"})
    if state.get("extra_pane"):
        panes.append({"pane_id":"w1:p2", "tab_id":"w1:t1", "workspace_id":"w1"})
    envelope({"panes": panes})
elif args[:2] == ["pane", "get"]:
    pane = args[2]
    human = pane == "w1:p2" and state.get("extra_pane")
    if pane != current_pane and not human:
        print("missing pane", file=sys.stderr)
        save()
        raise SystemExit(1)
    envelope({"pane": {
        "pane_id": pane,
        "workspace_id": current_workspace,
        "cwd": root,
        "agent": None if human or state.get("empty_shell") or (state.get("custom_harness") and not state.get("custom_reported")) else state.get("harness", "codex"),
        "agent_status": "unknown" if human or state.get("empty_shell") or (state.get("custom_harness") and not state.get("custom_reported")) else state.get("status", "idle"),
        "agent_session": None if human or state.get("empty_shell") or state.get("sessionless") or (state.get("custom_harness") and not state.get("report_custom_session")) else {"agent": state.get("harness", "codex"), "value": state.get("session_value_override", state.get("session_value", "session-1"))},
        "terminal_id": "term-human" if human else current_terminal,
        "tab_id": current_tab,
    }})
elif args[:2] == ["pane", "process-info"]:
    if state.get("fail_process_info"):
        print("injected process-info failure", file=sys.stderr)
        save()
        raise SystemExit(1)
    shell_pid = state["fixture_shell_pid"]
    shell_executable = state["fixture_shell_executable"]
    if state.get("empty_shell"):
        process_pid = shell_pid
        command = shell_executable
        parsed = [shell_executable]
        foreground_processes = [
            {"pid":process_pid, "name":os.path.basename(shell_executable),
             "cmdline":command, "argv":parsed, "executable":shell_executable}
        ]
        foreground_process_group_id = (
            process_pid + 1 if state.get("idle_shell_busy") else process_pid
        )
    else:
        command = ("/tmp/lookalike/muse" if state.get("wrong_custom_process")
                   else state.get("launch_command", "/usr/local/bin/muse")
                   if state.get("custom_harness") or not state.get("harness_pid")
                   # A native harness shows under its own name, as a real one does.
                   else "/usr/local/bin/" + state.get("harness", "codex"))
        parsed = shlex.split(command)
        executable = parsed[0]
        process_pid = state.get("custom_pid") or state.get("harness_pid") or 200
        foreground_processes = [] if state.get("custom_identity_hidden") else [
            {"pid":process_pid, "name":os.path.basename(executable),
             "cmdline":command, "argv":parsed, "executable":executable}
        ]
        foreground_process_group_id = (
            process_pid + 1 if state.get("wrong_custom_process_group")
            else process_pid if state.get("custom_pid") or state.get("harness_pid") else 200
        )
    for process in foreground_processes:
        if state.get("foreground_argv_mode") == "missing":
            process.pop("argv", None)
        elif state.get("foreground_argv_mode") == "null":
            process["argv"] = None
        elif state.get("foreground_argv_mode") == "empty":
            process["argv"] = []
        elif "foreground_argv_override" in state:
            process["argv"] = state["foreground_argv_override"]
    envelope({"process_info": {
        "pane_id":current_pane, "shell_pid":shell_pid,
        "foreground_process_group_id":foreground_process_group_id,
        "foreground_processes":foreground_processes,
    }})
elif args[:2] == ["tab", "create"]:
    state["closed"] = False
    state["tab_environment"] = [
        args[index + 1] for index, argument in enumerate(args[:-1])
        if argument == "--env"
    ]
    state["pane_id"] = "w1:p1"
    state["tab_id"] = "w1:t1"
    state["tab_label"] = args[args.index("--label") + 1] if "--label" in args else ""
    state["workspace_id"] = args[args.index("--workspace") + 1]
    envelope({"tab": {"tab_id": "w1:t1"}, "root_pane": {
        "pane_id":"w1:p1", "tab_id":"w1:t1", "workspace_id":"w1",
    }})
elif args[:2] == ["pane", "close"]:
    if args[2] != current_pane:
        raise SystemExit("refusing unexpected pane close")
    for key in ("custom_pid", "harness_pid"):
        if state.get(key):
            try:
                os.kill(state[key], signal.SIGTERM)
            except ProcessLookupError:
                pass
    state["closed"] = True
    state.setdefault("closed_panes", []).append(args[2])
elif args[:2] in (["agent", "focus"], ["tab", "focus"]):
    state["focused"] = args[2]
elif args[:2] == ["agent", "start"]:
    state["name"] = args[2]
    state["empty_shell"] = False
    state["harness"] = args[args.index("--kind") + 1]
    state["launch_arguments"] = args[args.index("--") + 1:]
    for selector in ("--session-id", "--resume", "resume"):
        if selector in state["launch_arguments"]:
            state["session_value"] = state["launch_arguments"][state["launch_arguments"].index(selector) + 1]
            break
    # A real process stands in for the harness so its kernel identity can be pinned.
    harness = subprocess.Popen(
        ["/bin/sleep", "60"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, start_new_session=True,
    )
    state["harness_pid"] = harness.pid
    if state.get("startup_timeout"):
        error = state.get("startup_error", {
            "id":"cli:agent:start", "error": {
                "code":"timeout", "message":"timed out waiting for agent startup",
            },
        })
        print(json.dumps(error), file=sys.stderr)
        save()
        raise SystemExit(1)
    if state.get("start_failure"):
        print("startup requires attention", file=sys.stderr)
        save()
        raise SystemExit(1)
elif args[:2] == ["pane", "run"]:
    state["launch_command"] = args[3]
    parsed = shlex.split(args[3])
    state["launch_arguments"] = parsed[1:]
    state["custom_harness"] = True
    if (state.get("report_custom_session") and len(state["launch_arguments"]) >= 2
            and state["launch_arguments"][-2] == "resume"):
        state["session_value"] = state["launch_arguments"][-1]
    if parsed and outside_case(parsed[0]):
        # The harness's rule for a program an edition runs, applied to the one this fixture runs.
        with open(os.path.join(root, "host-cli-guard.jsonl"), "a", encoding="utf-8") as stream:
            stream.write(json.dumps({
                "program": parsed[0], "argv": args, "cwd": os.getcwd(),
                "refused": "the fixture Herdr runs only a program inside its case directory",
            }) + "\n")
        print("cross harness host CLI guard: the fixture Herdr refused to run "
              f"{parsed[0]!r}, which is not inside the case directory", file=sys.stderr)
        save()
        raise SystemExit(127)
    child_command = (["/bin/sleep", "60"] if state.get("wrong_custom_process_identity")
                     else [parsed[0], "-c", "import time; time.sleep(60)"])
    child = subprocess.Popen(
        child_command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL, start_new_session=True,
    )
    state["custom_pid"] = child.pid
elif args[:2] == ["pane", "report-agent"]:
    state["harness"] = args[args.index("--agent") + 1]
    state["status"] = args[args.index("--state") + 1]
    state["custom_reported"] = True
elif args[:2] == ["agent", "get"]:
    if state.get("offline") or state.get("name") != args[2]:
        print("named agent unavailable", file=sys.stderr)
        save()
        raise SystemExit(1)
    envelope({"agent": {"name":args[2], "pane_id":current_pane, "tab_id":current_tab,
                        "terminal_id":current_terminal}})
elif args[:2] == ["agent", "list"]:
    agents = [] if state.get("closed") or not state.get("name") else [{
        "name": state["name"], "pane_id": current_pane, "tab_id": current_tab,
        "terminal_id": current_terminal,
    }]
    envelope({"agents": agents})
elif args[:2] == ["agent", "rename"]:
    if args[2] != current_pane:
        print("missing pane", file=sys.stderr)
        save()
        raise SystemExit(1)
    state["name"] = args[3]
    envelope({"agent": {"name": args[3], "pane_id": current_pane}})
elif args[:2] == ["tab", "get"]:
    if args[2] != current_tab or state.get("closed"):
        print("missing tab", file=sys.stderr)
        save()
        raise SystemExit(1)
    envelope({"tab": {"tab_id": current_tab, "label": state.get("tab_label", ""),
                      "workspace_id": current_workspace, "pane_count": 1}})
elif args[:2] == ["tab", "list"]:
    tabs = [] if state.get("closed") else [{
        "tab_id": current_tab, "label": state.get("tab_label", ""),
        "workspace_id": current_workspace,
    }]
    envelope({"tabs": tabs})
elif args[:2] == ["tab", "rename"]:
    if state.pop("fail_tab_rename_once", False):
        print("injected tab rename failure", file=sys.stderr)
        save()
        raise SystemExit(1)
    if args[2] != current_tab:
        print("missing tab", file=sys.stderr)
        save()
        raise SystemExit(1)
    state["tab_label"] = " ".join(args[3:])
    envelope({"tab": {"tab_id": current_tab, "label": state["tab_label"],
                      "workspace_id": current_workspace, "pane_count": 1}})
elif args[:2] == ["status", "server"]:
    print("status: running\nversion: 0.8.0\nprotocol: 20")
    if state.get("input_expect"):
        print("capabilities: input-expect")
elif args[:2] == ["pane", "report-agent-session"]:
    state["reported_session"] = args[args.index("--agent-session-id") + 1]
elif args[:2] == ["workspace", "get"]:
    workspace_id = args[2]
    envelope({"workspace": {
        "workspace_id": state.get("workspace_response_id", workspace_id),
        "label": "project-agents" if workspace_id == "w-project" else "project",
    }})
elif args[:2] == ["workspace", "list"]:
    envelope({"workspaces": [
        {"workspace_id":"w1", "label":"project"},
        {"workspace_id":"w-project", "label":"project-agents"},
    ]})
elif args[:2] == ["pane", "move"]:
    if args[2] != current_pane:
        print("missing source pane", file=sys.stderr)
        save()
        raise SystemExit(1)
    destination = args[args.index("--workspace") + 1]
    previous = current_pane
    state["pane_id"] = "w-project:p1"
    state["tab_id"] = "w-project:t1"
    if "--label" in args:
        state["tab_label"] = args[args.index("--label") + 1]
    state["workspace_id"] = destination
    envelope({"move_result": {
        "changed": True,
        "previous_pane_id": previous,
        "pane": {
            "pane_id": state["pane_id"],
            "tab_id": state["tab_id"],
            "workspace_id": destination,
        },
    }})
elif args[:2] == ["agent", "prompt"]:
    state.setdefault("submitted", []).append(args[3])
    after_write(args[3])
elif args[:2] == ["pane", "send-text"]:
    pasted = args[3]
    start, end = "\x1b[200~", "\x1b[201~"
    state["paste_wrapped"] = pasted.startswith(start) and pasted.endswith(end)
    state["custom_draft"] = pasted[len(start):-len(end)] if state["paste_wrapped"] else pasted
    state["custom_submitted"] = False
    if verified_composer():
        state["composer_draft"] = state.get("composer_draft", "") + state["custom_draft"]
    if state.get("custom_exit_after_send_text") and state.get("custom_pid"):
        state["retired_custom_pid"] = state["custom_pid"]
        try:
            os.kill(state["custom_pid"], signal.SIGTERM)
        except ProcessLookupError:
            pass
        state["custom_identity_hidden"] = True
elif args[:2] == ["agent", "wait"]:
    if state.get("goal_menu") and not state.get("confirmed_goal"):
        print("replacement menu requires confirmation", file=sys.stderr)
        save()
        raise SystemExit(1)
    if state.get("wait_mode") == "fail":
        print("working transition unavailable", file=sys.stderr)
        save()
        raise SystemExit(1)
    print(json.dumps({"result": {"agent": {"pane_id": args[2], "agent_status": "working"}}}, sort_keys=True))
elif args[:2] == ["pane", "read"]:
    if state.get("fail_read"):
        print("injected pane read failure", file=sys.stderr)
        save()
        raise SystemExit(1)
    if state.get("extra_pane_on_read"):
        state["extra_pane"] = True
    if state.get("restart_agent_on_read"):
        state["empty_shell"] = False
    if state.get("leave_idle_shell_on_read"):
        state["idle_shell_busy"] = True
    source = args[args.index("--source") + 1]
    if source == "visible" and state.get("terminal_on_visible_read"):
        state["terminal_id"] = state["terminal_on_visible_read"]
    if source == "visible" and state.get("screen"):
        sys.stdout.write(state["screen"])
    elif source == "visible" and verified_composer():
        sys.stdout.write(composer_screen())
    elif source == "visible" and state.get("custom_harness"):
        prefix = "Muse Code 1.3.0\n\nreasoning effort ultra is not available (gate ultra_reasoning_effort is closed); using xhigh\n"
        divider = "────────────────\n"
        history = "".join("❯ " + text + "\n◆ accepted\n" for text in state.get("submitted", []))
        footer = "watermelon-preview · xhigh · project · " + state.get(
            "custom_permission", "Auto-review") + "\n"
        if state.get("custom_submitted"):
            if state.get("custom_post_error"):
                sys.stdout.write(prefix + history + divider + "❯ " + state["custom_draft"] +
                                 "\nError: retry\n" + divider + footer)
            else:
                sys.stdout.write(prefix + history + "Working...\n" + divider + "❯\n" +
                                 divider + footer)
        elif state.get("custom_draft"):
            sys.stdout.write(prefix + history + divider + "❯ " + state["custom_draft"] +
                             "\n" + divider + footer)
        else:
            sys.stdout.write(prefix + history + divider + "❯\n" + divider + footer)
    elif source == "recent-unwrapped":
        if "read_unwrapped" in state:
            sys.stdout.write(state["read_unwrapped"])
        else:
            # A harness transcript shows each submitted prompt, which read-back looks for.
            sys.stdout.write("agent transcript\n" + "".join(
                text + "\n" for text in state.get("submitted", [])))
    else:
        sys.stdout.write(state.get("read_recent", "fallback transcript\n"))
elif args[:2] == ["pane", "send-keys"] and verified_composer() and state.get("composer_draft"):
    # Enter submits; Tab queues only while the agent is busy.
    if args[3] == "Enter" or (args[3] == "Tab" and state.get("status") == "working"):
        text = state.pop("composer_draft")
        state.setdefault("submitted", []).append(text)
        after_write(text)
elif args[:2] == ["pane", "send-keys"]:
    if state.get("custom_harness") and state.get("custom_draft") and args[3] == "Enter":
        state["custom_submitted"] = True
        if not state.get("custom_clear_without_submit"):
            state.setdefault("submitted", []).append(state["custom_draft"])
    else:
        state["confirmed_goal"] = args[3] == "Enter"
else:
    print("unsupported fake Herdr call: " + repr(args), file=sys.stderr)
    save()
    raise SystemExit(2)
save()
'''

_FAKE_MUSE_SKILLS = r'''import json, os, pathlib, shutil, sys
args = sys.argv[1:]
if args[:2] != ["skills", "install"] or len(args) < 8:
    print("unsupported fake Muse call", file=sys.stderr)
    raise SystemExit(2)
source = pathlib.Path(args[2])
if args[3:5] != ["--scope", "user"] or args[5] != "--name" or args[7] != "--json":
    print("invalid fake Muse install shape", file=sys.stderr)
    raise SystemExit(2)
name = args[6]
if name != "agentctl" or any(value not in ("--force",) for value in args[8:]):
    print("invalid fake Muse install options", file=sys.stderr)
    raise SystemExit(2)
config = os.environ.get("XDG_CONFIG_HOME")
if not config or not os.path.isabs(config):
    print("missing isolated XDG_CONFIG_HOME", file=sys.stderr)
    raise SystemExit(2)
destination = pathlib.Path(config) / "muse" / "skills" / name / "SKILL.md"
if destination.exists() and "--force" not in args:
    print("skill-already-installed", file=sys.stderr)
    raise SystemExit(1)
destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
shutil.copyfile(source / "SKILL.md", destination)
destination.chmod(0o600)
log = pathlib.Path(__file__).with_name("muse-skill-calls.jsonl")
with log.open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(args) + "\n")
print(json.dumps({"installed": {"id": name}}))
'''

# Host command-line tools an edition can reach by a bare name on PATH, for example the default
# native goal transport `codex app-server proxy`. A case that reaches one depends on the
# developer's machine: the real tool's output differs between the two runs (per-process paths)
# and can read or change live state of the owner's own agents. Every edition therefore runs with
# a guard directory first on PATH holding one stub per name. A stub records its call and fails;
# the harness reports every recorded call as a failure, so a case must hand both editions a
# fixture explicitly instead of passing on whatever the host has.
#
# Herdr is the exception a PATH stub cannot fully catch. With no `--herdr-bin`, the Rust edition
# searches PATH for `herdr` (and meets the stub), but the Python edition ignores PATH and runs
# `herdr` from fixed install locations (/usr/local/bin, /usr/bin, and bin directories under the
# account database's home directory). `herdr_refusal` therefore refuses, before spawning, every
# invocation that could reach Herdr without naming one inside the case directory.
#
# A stub cannot catch an executable named by an explicit path either. `host_cli_refusal` also
# refuses, before spawning, every invocation that names an executable outside the case directory
# through an option (see EXECUTABLE_OPTIONS and GOAL_COMMAND_OPTION) or through a goal command
# stored in the registry it reads (see STORED_GOAL_COMMAND_KEYS). The fixture Herdr applies the
# same rule to the custom harness program it runs for `pane run`, and the editions get an empty
# standard input, so a request read from it (the `agentctl mcp` server's) cannot name one either.
HOST_CLI_GUARDED = (
    "agentcloudctl", "agentterm", "agy", "claude", "codex", "gh", "herdr", "muse", "opencode",
    "tmux", "wrkslots",
)
HOST_CLI_GUARD_DIRECTORY = "host-cli-guard"
HOST_CLI_GUARD_LOG = "host-cli-guard.jsonl"
_HOST_CLI_GUARD = r'''import json, os, pathlib, sys
stub = pathlib.Path(__file__)
log = stub.resolve().parent.parent / "host-cli-guard.jsonl"
with log.open("a", encoding="utf-8") as stream:
    stream.write(json.dumps({"program": stub.name, "argv": sys.argv[1:], "cwd": os.getcwd()}) + "\n")
print(f"cross harness host CLI guard: refused to run the host {stub.name}", file=sys.stderr)
raise SystemExit(127)
'''


def guarded_path(root: Path, path: str | None) -> str:
    """Put one case root's host CLI guard ahead of every other PATH entry."""
    guard = str(root / HOST_CLI_GUARD_DIRECTORY)
    return guard if not path else guard + os.pathsep + path


# Environment variables that name a host executable outright (the Python edition's headless
# runner reads them in agentctl/foreign/lib.py, wrkslots discovery in agentctl/subagents.py). An
# absolute path in one would bypass the stubs above, so no edition inherits them from the caller:
# an edition that then looks the tool up by its bare name meets the stub and is reported.
AMBIENT_EXECUTABLE_OVERRIDES = {
    "AGENTCTL_WRKSLOTS_BIN": "wrkslots", "AGY_BIN": "agy", "CODEX_BIN": "codex",
    "HERDR_BIN": "herdr", "MUSE_BIN": "muse",
}


def without_ambient_executables(environment: dict[str, str]) -> dict[str, str]:
    """Drop every caller-supplied host executable override from an edition's environment."""
    for variable in AMBIENT_EXECUTABLE_OVERRIDES:
        environment.pop(variable, None)
    return environment


# The Git both editions run, and the configuration that overrides whatever a repository sets:
# check-ignore reads the index, and reading the index runs the core.fsmonitor helper.
SYSTEM_GIT = "/usr/bin/git"
GIT_COMMAND_CONFIGURATION = (("core.fsmonitor", "false"),)


def with_case_git(environment: dict[str, str], root: Path) -> dict[str, str]:
    """Give an edition's Git no configuration but the case's own repository.

    Both editions run the system Git (`git check-ignore` in agentctl/profiles.py and
    profiles.rs), and Git runs helper programs that its configuration names, such as
    core.fsmonitor, by absolute path where no PATH stub sees them. So no `GIT_*` variable is
    inherited (GIT_CONFIG_COUNT with GIT_CONFIG_KEY_n/GIT_CONFIG_VALUE_n, and
    GIT_CONFIG_PARAMETERS, add configuration; GIT_DIR and its relatives move the repository),
    the system and global configuration files are not read (the XDG one is already the case's,
    and GIT_CONFIG_GLOBAL replaces it too), and repository discovery stops at the case directory,
    so a repository that encloses the harness directory is not read either. The repository's own
    configuration is still read, along with anything it includes, so GIT_COMMAND_CONFIGURATION
    is given last, at command scope, where it overrides every file. And Git fetches no object
    that a promisor remote should supply: the fetch would run the remote's transport program,
    such as remote.<name>.uploadpack, and reading a skip-worktree `.gitignore` can start one.
    """
    for variable in [name for name in environment if name.startswith("GIT_")]:
        del environment[variable]
    environment["GIT_CONFIG_NOSYSTEM"] = "1"
    environment["GIT_CONFIG_GLOBAL"] = os.devnull
    environment["GIT_CEILING_DIRECTORIES"] = os.path.dirname(os.path.realpath(root))
    environment["GIT_NO_LAZY_FETCH"] = "1"
    environment["GIT_CONFIG_COUNT"] = str(len(GIT_COMMAND_CONFIGURATION))
    for index, (key, value) in enumerate(GIT_COMMAND_CONFIGURATION):
        environment[f"GIT_CONFIG_KEY_{index}"] = key
        environment[f"GIT_CONFIG_VALUE_{index}"] = value
    return environment


def init_case_repository(root: Path) -> None:
    """Create the repository that a case's editions run Git in, from Git's built-in template.

    `git init` copies a template directory into the new `.git`, including any `config` file
    there, and the caller chooses that directory with GIT_TEMPLATE_DIR or init.templateDir. So
    the harness creates every case repository with the Git environment the editions get.
    """
    subprocess.run(
        [SYSTEM_GIT, "init", "-q", str(root)],
        check=True,
        stdin=subprocess.DEVNULL,
        env=with_case_git(dict(os.environ), root),
    )


FIXTURE_HERDR = ("--herdr-bin", "<HERDR>")
# Commands that print text or read local configuration and never construct a Herdr client. Any
# other command, including an unknown one, must name the fixture Herdr.
HERDR_FREE_COMMANDS = frozenset((
    "--help", "-h", "--version", "-V", "--userguide",
    "capabilities", "profiles", "quickstart", "skill", "userguide",
))
# Every option that either agentctl edition accepts before the command and that takes a value: the
# Python root parser's three, and the Rust clap globals (rs/agentctl/src/cli.rs, `struct Cli`),
# which add the agentcloud ones. A test reads both parsers so that a new one cannot hide the
# command from herdr_refusal. Their other options before the command are in HERDR_FREE_COMMANDS.
_GLOBAL_VALUE_OPTIONS = (
    "--registry", "--state", "--herdr-bin", "--agentcloudctl-bin", "--agentterm-bin",
    "--agentcloud-url", "--from-session",
)
# Options whose value an edition executes, with the program each one names. The Rust edition
# alone has `--agentcloudctl-bin`, `--agentterm-bin` and the `inbox watch` option `--claude-bin`
# (which runs `claude agents --json`): the Python edition rejects them as unknown, but the Rust
# edition may already have run the program by then.
EXECUTABLE_OPTIONS = {
    "--herdr-bin": "herdr", "--agentcloudctl-bin": "agentcloudctl", "--agentterm-bin": "agentterm",
    "--claude-bin": "claude",
}
# The native goal transport: a JSON array that an edition runs as an argv, its first element
# unresolved and so never looked up on PATH when it contains a `/`.
GOAL_COMMAND_OPTION = "--goal-command-json"
# The only goal transport an edition may run, explicit or stored: the case's fake Herdr in the
# mode that answers goal requests on its standard input (`["<HERDR>", "goal-rpc"]`).
GOAL_TRANSPORT_FIXTURE = "fake-herdr"
GOAL_TRANSPORT_ARGUMENTS = ("goal-rpc",)
GOAL_TRANSPORT_DESCRIPTION = (
    f"the case's fixture goal transport (./{GOAL_TRANSPORT_FIXTURE} "
    f"{' '.join(GOAL_TRANSPORT_ARGUMENTS)})"
)
# The registry directory option (`--state` is its alias in both agentctl editions) and each
# edition's default, relative to the working directory: `.agentctl` for agentctl and
# `.herdr-agents` for the herdr-agent compatibility command.
REGISTRY_OPTIONS = ("--registry", "--state")
DEFAULT_REGISTRIES = (".agentctl", ".herdr-agents")
# Record fields that hold a goal transport the editions run when the command line names none:
# `goal_command` in a flat record, `native_command` under `goal` in nested storage.
STORED_GOAL_COMMAND_KEYS = frozenset(("goal_command", "native_command"))
# Commands that ignore `--herdr-bin`: the Python `agentctl chat` builds a default HerdrClient,
# which runs the installed Herdr, so naming the fixture cannot redirect it.
HERDR_BIN_IGNORED_COMMANDS = frozenset(("chat",))


def _names_case_file(root: Path, value: str) -> bool:
    """Whether an executable value is a file inside the case directory, not an installed name.

    The editions resolve a relative value against their working directory, the case root, whose
    os.getcwd() is its physical path. They canonicalize it in two different ways. The kernel and
    the Rust client follow each symbolic link before a later `..` (os.path.realpath). The Python
    client first removes `..` lexically (os.path.abspath, in client._validated_executable) and
    only then follows links. Given `link -> nested/deeper`, `./link/../herdr` is `nested/herdr`
    to the first and `./herdr` to the second. A value counts only if both readings stay inside
    the case directory.
    """
    return os.sep in value and _inside_case(root, value)


def _inside_case(root: Path, value: str) -> bool:
    """Whether a path, relative to the case root, stays inside it under both readings of `..`."""
    real_root = os.path.realpath(root)
    joined = value if os.path.isabs(value) else os.path.join(real_root, value)
    readings = (os.path.realpath(joined), os.path.realpath(os.path.normpath(joined)))
    return all(os.path.commonpath((reading, real_root)) == real_root for reading in readings)


def _option_values(tokens: Sequence[str], option: str) -> list[str]:
    """Every value given to a long option, in its spaced and in its `=` form."""
    return [
        tokens[index + 1] for index, value in enumerate(tokens[:-1]) if value == option
    ] + [value.split("=", 1)[1] for value in tokens if value.startswith(f"{option}=")]


def _goal_command_refusal(root: Path, value: str) -> str | None:
    """Say why a `--goal-command-json` value could run a program outside the case directory."""
    try:
        command: object = json.loads(value)
    except ValueError:
        return f"{GOAL_COMMAND_OPTION} {value!r} is not JSON, so its program cannot be checked"
    if _runs_goal_fixture(root, command):
        return None
    return f"{GOAL_COMMAND_OPTION} {value!r} is not {GOAL_TRANSPORT_DESCRIPTION}"


def _runs_goal_fixture(root: Path, command: object) -> bool:
    """Whether a decoded goal command is empty or is exactly the case's fixture goal transport.

    Naming some file inside the case directory is not enough: every case also holds
    `fake-muse-runtime`, a copy of the Python interpreter, which runs whatever program its
    arguments give it. So the program must be GOAL_TRANSPORT_FIXTURE under both readings of `..`
    (see _names_case_file), reached directly or through links inside the case, and the arguments
    must be exactly GOAL_TRANSPORT_ARGUMENTS, the mode in which it answers requests on its
    standard input and starts no process.
    """
    if command == []:
        return True  # both editions reject an empty command before running anything
    if not (
        isinstance(command, list)
        and all(isinstance(word, str) for word in command)
        and tuple(command[1:]) == GOAL_TRANSPORT_ARGUMENTS
        and _names_case_file(root, command[0])
    ):
        return False
    real_root = os.path.realpath(root)
    fixture = os.path.join(real_root, GOAL_TRANSPORT_FIXTURE)
    program: str = command[0]
    joined = program if os.path.isabs(program) else os.path.join(real_root, program)
    readings = (os.path.realpath(joined), os.path.realpath(os.path.normpath(joined)))
    return all(reading == fixture for reading in readings)


def _stored_goal_commands(document: bytes) -> list[object]:
    """Every value of a STORED_GOAL_COMMAND_KEYS field at any depth, duplicate keys included."""
    found: list[object] = []

    def pairs(items: list[tuple[str, object]]) -> dict[str, object]:
        found.extend(value for key, value in items if key in STORED_GOAL_COMMAND_KEYS)
        return dict(items)

    try:
        json.loads(document, object_pairs_hook=pairs)
    except ValueError:
        # Not JSON to this parser, so not to the Python edition, which reads records with the same
        # parser, or to the Rust edition, whose parser accepts less: neither can run a command
        # from it.
        return []
    return found


def stored_goal_command_refusal(root: Path, arguments: Sequence[str]) -> str | None:
    """Say why a goal command stored in the registry an invocation reads could run a host program.

    Without `--goal-command-json` an edition runs the command stored in the agent record, so a
    seeded record is as much an input as the command line. Every registry the invocation could
    read must stay inside the case directory: each value of a REGISTRY_OPTIONS option anywhere,
    and both defaults always, because a token that only looks like the option (text after a `--`)
    leaves the default in effect. Each is read under both readings of `..` (see
    _names_case_file): `./link/../registry` is one directory to the kernel and another to an
    edition that removes `..` first. Every directory and record file under it must stay inside
    too, symbolic links followed; a directory that cannot be listed is refused, since an edition
    may still open a record in it by name. Every stored command must be null, empty, or the case's
    fixture goal transport (_runs_goal_fixture).
    """
    registries = [
        *(value for option in REGISTRY_OPTIONS for value in _option_values(arguments, option)),
        *DEFAULT_REGISTRIES,
    ]
    for registry in registries:
        if not _inside_case(root, registry):
            return (
                f"registry {registry!r} is outside the case directory, so the goal commands "
                "stored there cannot be checked"
            )
    real_root = os.path.realpath(root)
    visited: set[str] = set()
    for registry in registries:
        joined = registry if os.path.isabs(registry) else os.path.join(real_root, registry)
        for start in dict.fromkeys((joined, os.path.normpath(joined))):
            errors: list[OSError] = []
            for directory, subdirectories, files in os.walk(
                start, onerror=errors.append, followlinks=True
            ):
                if os.path.realpath(directory) in visited:
                    subdirectories.clear()  # a link back to a directory already read
                    continue
                visited.add(os.path.realpath(directory))
                for path in (directory, *(os.path.join(directory, name) for name in files)):
                    if not _inside_case(root, path):
                        return (
                            f"registry {registry!r} links to {os.path.realpath(path)!r} outside "
                            "the case directory, so the goal commands stored there cannot be "
                            "checked"
                        )
                for name in files:
                    record = os.path.join(directory, name)
                    # Records are `agent.json` in both editions; a FIFO or a dangling link is no
                    # record.
                    if not name.endswith(".json") or not os.path.isfile(record):
                        continue
                    try:
                        with open(record, "rb") as stream:
                            document = stream.read()
                    except OSError:
                        continue  # unreadable to the editions too: they run as the same user
                    for command in _stored_goal_commands(document):
                        if command is not None and not _runs_goal_fixture(root, command):
                            return (
                                f"{record!r} stores the goal command {command!r}, which is not "
                                f"{GOAL_TRANSPORT_DESCRIPTION}"
                            )
            for error in errors:
                # A registry that does not exist yet stores nothing; any other failure to list a
                # directory leaves records in it unread.
                if not (isinstance(error, FileNotFoundError) and error.filename == start):
                    return (
                        f"registry {registry!r} could not be read completely ({error}), so the "
                        "goal commands stored there cannot be checked"
                    )
    return None


def host_cli_refusal(root: Path, arguments: Sequence[str]) -> tuple[str, str] | None:
    """Name the host program an invocation could run, and why, or return None if it cannot.

    Three kinds of reach are refused before either edition starts. An option that names an
    executable (EXECUTABLE_OPTIONS, GOAL_COMMAND_OPTION) must name a file inside the case
    directory wherever it appears, before or after a `--`: after a root-level `--`, the Python
    subcommand parser reads its remaining tokens as options again, so a later token is not always
    text. A goal command stored in the registry must too (`stored_goal_command_refusal`). Then
    `herdr_refusal` refuses an invocation that would run the installed Herdr by default. The
    check is deliberately conservative: it may refuse an invocation that would not have run the
    program, never the reverse.
    """
    for option, program in EXECUTABLE_OPTIONS.items():
        for value in _option_values(arguments, option):
            if not _names_case_file(root, value):
                return program, f"{option} {value!r} is not a fixture inside the case directory"
    for value in _option_values(arguments, GOAL_COMMAND_OPTION):
        reason = _goal_command_refusal(root, value)
        if reason is not None:
            return "goal-command", reason
    reason = stored_goal_command_refusal(root, arguments)
    if reason is not None:
        return "goal-command", reason
    reason = herdr_refusal(root, arguments)
    return None if reason is None else ("herdr", reason)


def herdr_refusal(root: Path, arguments: Sequence[str]) -> str | None:
    """Say why an invocation would run the installed Herdr by default, or None if it would not.

    Call it through `host_cli_refusal`, which first refuses every `--herdr-bin` that names
    something outside the case directory. Tokens after the first `--` may be positional text, so
    a `--herdr-bin` there never admits an invocation, and a help flag there asks for nothing. The
    command is the first token after the global options (_GLOBAL_VALUE_OPTIONS, spaced or `=`);
    the Rust edition accepts more of them than the Python one, so `--from-session=ID chat` is a
    Chat command to it.
    """
    options_end = arguments.index("--") if "--" in arguments else len(arguments)
    named = _option_values(arguments[:options_end], "--herdr-bin")
    index = 0
    while index < len(arguments):
        if index == options_end:
            index += 1
        elif index < options_end and arguments[index] in _GLOBAL_VALUE_OPTIONS:
            index += 2
        elif index < options_end and arguments[index].startswith(
            tuple(f"{option}=" for option in _GLOBAL_VALUE_OPTIONS)
        ):
            index += 1
        else:
            break
    if index >= len(arguments):
        return None
    if index < options_end and arguments[index] in HERDR_FREE_COMMANDS:
        return None
    if index < options_end and arguments[index].startswith("-"):
        # An option that is not a global of either agentctl edition, so where the command starts
        # is unknown: a command that ignores --herdr-bin may follow it.
        for later in arguments[index + 1:]:
            if later in HERDR_BIN_IGNORED_COMMANDS:
                return (
                    f"{later!r} follows the unrecognized option {arguments[index]!r}, so it may "
                    "be the command, which ignores --herdr-bin and runs the Herdr installed on "
                    "this host"
                )
    if index + 1 < options_end and arguments[index + 1] in ("--help", "-h"):
        return None
    if arguments[index] in HERDR_BIN_IGNORED_COMMANDS:
        return f"{arguments[index]!r} ignores --herdr-bin and runs the Herdr installed on this host"
    if named:
        return None
    return (
        f"{arguments[index]!r} has no --herdr-bin, so the editions would run the Herdr installed "
        "on this host"
    )


class Harness:
    """Create paired fixtures and invoke both implementations."""

    def __init__(self, root: Path, python: Sequence[str], rust: Sequence[str]) -> None:
        self.root = root
        self.python = tuple(python)
        self.rust = tuple(rust)
        self.serial = 0
        self.case_roots: list[Path] = []
        shell = os.path.realpath("/bin/bash")
        self.fixture_shell = subprocess.Popen(
            [shell, "--noprofile", "--norc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.replacement_shell = subprocess.Popen(
            [shell, "--noprofile", "--norc"],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.fixture_shell_executable = shell

    def close(self) -> None:
        """Reap only the two shell generations created by this differential."""
        for process in (self.fixture_shell, self.replacement_shell):
            if process.stdin is not None:
                process.stdin.close()
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

    def case(self, label: str, state: Mapping[str, object] | None = None) -> PairCase:
        self.serial += 1
        safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", label).strip("-") or "case"
        base = self.root / f"{self.serial:03d}-{safe}"
        python_root = base / "python" / "project"
        rust_root = base / "rust" / "project"
        initial: dict[str, object] = {
            "status": "idle",
            "calls": [],
            "submitted": [],
            "fixture_shell_pid": self.fixture_shell.pid,
            "fixture_shell_executable": self.fixture_shell_executable,
        }
        if state is not None:
            initial.update(state)
        for root in (python_root, rust_root):
            root.mkdir(parents=True, mode=0o700)
            fixture = root / "fake-herdr"
            fixture.write_text(f"#!{sys.executable}\n{_FAKE_HERDR}", encoding="utf-8")
            fixture.chmod(0o700)
            fake_muse = root / "fake-muse-skills"
            fake_muse.write_text(
                f"#!{sys.executable}\n{_FAKE_MUSE_SKILLS}", encoding="utf-8"
            )
            fake_muse.chmod(0o700)
            fake_muse_runtime = root / "fake-muse-runtime"
            shutil.copyfile(sys.executable, fake_muse_runtime)
            fake_muse_runtime.chmod(0o700)
            (root / "state.json").write_text(
                json.dumps(initial, sort_keys=True), encoding="utf-8"
            )
            guard = root / HOST_CLI_GUARD_DIRECTORY
            guard.mkdir(mode=0o700)
            for program in HOST_CLI_GUARDED:
                stub = guard / program
                stub.write_text(f"#!{sys.executable}\n{_HOST_CLI_GUARD}", encoding="utf-8")
                stub.chmod(0o700)
            self.case_roots.append(root)
        return PairCase(python_root, rust_root)

    def host_cli_calls(self) -> list[str]:
        """Describe every host CLI guard call recorded in any case root so far."""
        calls: list[str] = []
        for root in self.case_roots:
            log = root / HOST_CLI_GUARD_LOG
            if not log.exists():
                continue
            case = root.relative_to(self.root)
            for line in log.read_text(encoding="utf-8").splitlines():
                calls.append(f"{case}: {line}")
        return calls

    @staticmethod
    def refuse_host_cli(root: Path, argv: Sequence[str]) -> str | None:
        """Record and return the refusal for an invocation that could reach a host program."""
        refusal = host_cli_refusal(root, argv)
        if refusal is None:
            return None
        program, reason = refusal
        with (root / HOST_CLI_GUARD_LOG).open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({
                "program": program, "argv": list(argv), "cwd": str(root), "refused": reason,
            }) + "\n")
        return reason

    def require_no_host_cli(self, report: Report) -> None:
        """Fail the report when any edition, in any case, reached a guarded host CLI."""
        calls = self.host_cli_calls()
        report.require(
            "harness/no-host-cli", not calls,
            f"{len(calls)} call(s) reached a host CLI instead of a fixture: {calls!r}",
        )

    def invoke(self, case: PairCase, arguments: Sequence[str]) -> tuple[Outcome, Outcome]:
        return (
            self._invoke_one(self.python, case.python_root, arguments),
            self._invoke_one(self.rust, case.rust_root, arguments),
        )

    def _invoke_one(
        self, command: Sequence[str], root: Path, arguments: Sequence[str]
    ) -> Outcome:
        expanded = [
            value.replace("<ROOT>", str(root)).replace("<HERDR>", str(root / "fake-herdr"))
            for value in arguments
        ]
        refusal = self.refuse_host_cli(root, expanded)
        if refusal is not None:
            return _normalize(Outcome(
                127, "", f"cross harness host CLI guard: refused to run the editions: {refusal}\n",
            ), root)
        environment = with_case_git(without_ambient_executables(dict(os.environ)), root)
        existing = environment.get("PYTHONPATH", "")
        local = str(REPO_ROOT / "py")
        environment["PYTHONPATH"] = local if not existing else local + os.pathsep + existing
        environment.update({"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "NO_COLOR": "1", "HERDR_WORKSPACE_ID": "w1"})
        for harness_name in ("CODEX", "CLAUDE"):
            environment[f"AGENTCTL_{harness_name}_SKILLS_DIR"] = str(
                root / "skill-homes" / harness_name.lower()
            )
        fixture_state = _state(root)
        environment["PATH"] = guarded_path(
            root,
            str(root / "hostile-bin") if fixture_state.get("hostile_path")
            else environment.get("PATH"),
        )
        environment["AGENTCTL_MUSE_BIN"] = (
            str(root / "missing-muse")
            if fixture_state.get("missing_custom_executable")
            else str(root / "fake-muse-skills")
            if tuple(arguments[:2]) == ("skill", "install")
            else str(root / "fake-muse-runtime")
        )
        environment["XDG_CONFIG_HOME"] = str(root / "xdg-config")
        empty_environment = fixture_state.get("empty_environment", [])
        if not isinstance(empty_environment, list) or any(
            not isinstance(variable, str) for variable in empty_environment
        ):
            raise TypeError("empty_environment fixture must contain strings")
        if any(variable.startswith("GIT_") for variable in empty_environment):
            # An empty value would undo with_case_git: GIT_NO_LAZY_FETCH= turns fetching back on.
            raise TypeError("empty_environment fixture may not change the editions' Git variables")
        for variable in empty_environment:
            environment[variable] = ""
        try:
            completed = subprocess.run(
                [*command, *expanded],
                cwd=root,
                env=environment,
                # Not the caller's: the `agentctl mcp` server would read requests from it, and a
                # request can name a goal command after the guard has checked the command line.
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
                timeout=TIMEOUT_SECONDS,
            )
            outcome = Outcome(completed.returncode, completed.stdout, completed.stderr)
        except subprocess.TimeoutExpired as error:
            outcome = Outcome(
                124,
                _decode(error.stdout),
                _decode(error.stderr) + "\nTIMEOUT\n",
            )
        return _normalize(outcome, root)


def _decode(value: bytes | str | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return value


def _normalize(outcome: Outcome, root: Path) -> Outcome:
    def text(value: str) -> str:
        return _MESSAGE_ID.sub("<MESSAGE_ID>", value.replace(str(root), "<ROOT>"))

    return Outcome(outcome.returncode, text(outcome.stdout), text(outcome.stderr))


def _state(root: Path) -> dict[str, object]:
    raw: object = json.loads((root / "state.json").read_text(encoding="utf-8"))
    if not isinstance(raw, dict):
        raise AssertionError("fake Herdr state is not an object")
    return {str(key): value for key, value in raw.items()}


def _queue_snapshot(root: Path, queue: str) -> dict[str, object]:
    queue_root = root / queue
    snapshot: dict[str, object] = {}
    if not queue_root.exists():
        return snapshot
    for path in sorted(queue_root.rglob("*")):
        relative = path.relative_to(queue_root).as_posix()
        if path.is_dir() or relative.startswith(".") or relative == "target.json":
            continue
        if path.suffix == ".json" or path.name.endswith(".json.error"):
            try:
                value: object = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, UnicodeError, json.JSONDecodeError):
                snapshot[_MESSAGE_ID.sub("<MESSAGE_ID>", relative)] = path.read_bytes().hex()
                continue
            if isinstance(value, dict):
                for key in (
                    "queued_at",
                    "inflight_at",
                    "confirmed_at",
                    "delivery_failed_at",
                    "delivery_blocked_at",
                    "failed_at",
                ):
                    value.pop(key, None)
                if isinstance(value.get("id"), str):
                    value["id"] = _MESSAGE_ID.sub("<MESSAGE_ID>", str(value["id"]))
                if isinstance(value.get("artifact"), str):
                    value["artifact"] = _MESSAGE_ID.sub(
                        "<MESSAGE_ID>", str(value["artifact"])
                    )
            snapshot[_MESSAGE_ID.sub("<MESSAGE_ID>", relative)] = value
    return snapshot


def _revive_document(path: Path) -> dict[str, object]:
    value: object = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise AssertionError(f"recovery fixture is not an object: {path.name}")
    return {str(key): item for key, item in value.items()}


def _revive_tree(path: Path) -> dict[str, tuple[int, int, int, bytes | None]]:
    """Capture all entries, including binding, lock, audit and quarantine artifacts."""
    if not path.exists():
        return {}
    result: dict[str, tuple[int, int, int, bytes | None]] = {}
    for entry in (path, *sorted(path.rglob("*"))):
        metadata = entry.lstat()
        if not stat.S_ISREG(metadata.st_mode) and not stat.S_ISDIR(metadata.st_mode):
            raise AssertionError("recovery fixture must contain only regular files and directories")
        result[entry.relative_to(path).as_posix()] = (
            metadata.st_dev, metadata.st_ino, stat.S_IMODE(metadata.st_mode),
            entry.read_bytes() if stat.S_ISREG(metadata.st_mode) else None,
        )
    return result


def _revive_pane_state(root: Path, values: Mapping[str, object]) -> None:
    state = _state(root)
    panes = state.get("revive_panes")
    if not isinstance(panes, dict) or not isinstance(panes.get("w1:p1"), dict):
        raise AssertionError("recovery fixture has no old pane")
    panes["w1:p1"].update(values)
    (root / "state.json").write_text(json.dumps(state, sort_keys=True), encoding="utf-8")


def _revive_dry_run(
    harness: Harness, report: Report, root: Path, command: Sequence[str],
    label: str, *, batch: bool, action: str,
) -> None:
    before = _revive_tree(root / "registry")
    initial_state = _state(root)
    effect_keys = ("revive_launch_count", "revive_tabs_created", "closed_panes", "revive_input_calls")
    arguments = ("revive", "--all" if batch else "worker", "--dry-run",
                 "--registry", "<ROOT>/registry", *FIXTURE_HERDR)
    outcome = harness._invoke_one(command, root, arguments)
    try:
        value: object = json.loads(outcome.stdout)
    except ValueError:
        value = None
    plans = value.get("agents") if isinstance(value, dict) and batch else [value]
    report.require(
        label + "/plan",
        outcome.returncode == 0 and isinstance(plans, list) and len(plans) == 1
        and isinstance(plans[0], dict) and plans[0].get("action") == action,
        f"expected one {action} plan: {outcome!r}",
    )
    after = _state(root)
    report.require(
        label + "/immutable",
        _revive_tree(root / "registry") == before
        and all(after.get(key) == initial_state.get(key) for key in effect_keys),
        "dry-run changed a record, queue, directory identity, runtime or presentation",
    )


def _revive_interop(harness: Harness, report: Report) -> None:
    """Recover each edition's producer bytes through the other canonical CLI."""
    common = ("--registry", "<ROOT>/registry", *FIXTURE_HERDR)
    expected_arguments = [
        "resume", "session-1", "--no-alt-screen", "--model", "recovery-model",
        "--config", "model_reasoning_effort=high", "--sandbox", "read-only",
    ]
    directions = (
        ("python-to-rust", harness.python, harness.rust),
        ("rust-to-python", harness.rust, harness.python),
    )
    for direction, producer, consumer in directions:
        for phase in ("ready", "published"):
            label = f"primary/revive/{direction}/{phase}"
            processes: list[subprocess.Popen[bytes]] = []
            try:
                for _ in range(2):
                    processes.append(subprocess.Popen(
                        ["/bin/sleep", "600"], stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                        start_new_session=True,
                    ))
                old_process, new_process = processes
                case = harness.case(label, {
                    "revive_mode": True, "input_expect": True,
                    "revive_harness_pids": [old_process.pid, new_process.pid],
                })
                root = case.python_root if producer == harness.python else case.rust_root
                init_case_repository(root)
                (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
                configuration = root / ".agentctl"
                configuration.mkdir(mode=0o700)
                profile = configuration / "profiles.json"
                profile.write_text(json.dumps({
                    "schema": "agentctl-profiles/v1", "profiles": {"recovery": {
                        "harness": "codex", "mode": "interactive", "model": "recovery-model",
                        "reasoning_effort": "high", "argv": ["--sandbox", "read-only"], "env": {},
                    }},
                }), encoding="utf-8")
                profile.chmod(0o600)
                started = harness._invoke_one(producer, root, (
                    "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
                    "--profile", "recovery", *common,
                ))
                report.require(label + "/start", started.returncode == 0,
                               f"producer failed to create the original record: {started!r}")
                record_path = root / "registry/worker/agent.json"
                if started.returncode != 0 or not record_path.exists():
                    continue
                record = _revive_document(record_path)
                identity = record.get("harness_identity")
                report.require(
                    label + "/kernel-anchor",
                    isinstance(identity, dict) and identity.get("pid") == old_process.pid
                    and old_process.poll() is None and new_process.pid != old_process.pid,
                    "original harness was not pinned to the separate live fixture child",
                )
                _revive_pane_state(root, {"status": "working"})
                queued = harness._invoke_one(producer, root, (
                    "send", "worker", "old queued instruction", "--message-id", "pending-work",
                    "--ready-timeout", "0", *common,
                ))
                queue = root / "registry/worker/queue"
                report.require(
                    label + "/queued-before-death",
                    queued.returncode == 75 and (queue / "inbox/pending-work.json").is_file()
                    and not _state(root).get("revive_input_calls"),
                    f"busy target did not retain the safe pending prompt: {queued!r}",
                )
                if queued.returncode != 75 or not queue.exists():
                    continue
                artifacts = {
                    "inflight/interrupted.json": b'{"text":"uncertain inflight instruction"}\n',
                    "failed/uncertain.json": b'{"possibly_submitted":true,"text":"never replay"}\n',
                    "quarantine/raw.json.error": b"opaque quarantine evidence\x00\xff\n",
                    ".queue-audit": b"keep all private queue artifacts\n",
                }
                for relative, content in artifacts.items():
                    artifact = queue / relative
                    artifact.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                    artifact.write_bytes(content)
                    artifact.chmod(0o600)
                # Fixed values expose lossy JSON float parsing instead of relying
                # on the wall clock to happen to produce a sensitive timestamp.
                record.update({"created_at": 1791594983.6648757,
                               "goal": "finish the recorded task", "paused": True,
                               "recovery_note": {"task": "last recorded instruction",
                                   "numeric": {"precise_number": 1791594983.6648757,
                                               "values": [1.2345678901234567, -1791594983.6648757]}}})
                record_path.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
                old_bytes = record_path.read_bytes()
                old_queue = _revive_tree(queue)
                old_directory = record_path.parent.stat()
                _revive_dry_run(harness, report, root, consumer, label + "/live-dry-run",
                                batch=False, action="skip")
                old_process.kill()
                old_process.wait(timeout=5)
                report.require(label + "/actual-harness-death", old_process.returncode == -9,
                               "the fixture did not kill and reap its original pinned harness")
                _revive_pane_state(root, {
                    "empty_shell": True, "terminal_id": "restored-old-terminal", "label": "",
                })
                _revive_dry_run(harness, report, root, producer, label + "/dead-dry-run",
                                batch=False, action="revive")
                _revive_dry_run(harness, report, root, consumer, label + "/all-dry-run",
                                batch=True, action="revive")
                state = _state(root)
                state["revive_fail_old_read_once" if phase == "ready"
                      else "revive_fail_old_close_once"] = True
                (root / "state.json").write_text(json.dumps(state, sort_keys=True), encoding="utf-8")
                old_token = record.get("token")
                if not isinstance(old_token, str):
                    raise AssertionError("producer record has no generation token")
                interrupted = harness._invoke_one(producer, root, (
                    "revive", "worker", "--expected-token", old_token, *common,
                ))
                journal_path = root / "registry/.revives" / f"{old_token}.json"
                report.require(
                    label + "/interruption", interrupted.returncode == 75 and journal_path.is_file(),
                    f"producer did not retain a recovery journal after interruption: {interrupted!r}",
                )
                if not journal_path.is_file():
                    continue
                journal = _revive_document(journal_path)
                report.require(label + "/durable-phase", journal.get("phase") == phase,
                               f"expected {phase} journal, got {journal.get('phase')!r}")
                operation = journal_path.parent / old_token
                archive = root / "registry/archive" / f"worker-{old_token}"
                candidate_path = operation / "new/agent.json" if phase == "ready" else record_path
                if not candidate_path.is_file():
                    report.require(label + "/candidate-present", False,
                                   "interruption lost the staged or published candidate")
                    continue
                ready_bytes = candidate_path.read_bytes()
                candidate = _revive_document(candidate_path)
                stopped_bytes = (operation / "stopped.json").read_bytes()
                expected_stopped = {**record, "lifecycle": "stopped"}
                report.require(
                    label + "/stopped-only-lifecycle",
                    json.loads(stopped_bytes) == expected_stopped,
                    "prepared stopped record changed original metadata beyond its lifecycle",
                )
                report.require(
                    label + "/candidate-unknown-numeric-metadata",
                    candidate.get("recovery_note") == record.get("recovery_note"),
                    "candidate changed the saved task or nested unknown numeric metadata",
                )
                report.require(
                    label + "/producer-byte-digests",
                    journal.get("schema") == "agentctl-revive/v1"
                    and journal.get("old_record_sha256") == hashlib.sha256(old_bytes).hexdigest()
                    and journal.get("ready_record_sha256") == hashlib.sha256(ready_bytes).hexdigest()
                    and journal.get("stopped_record_sha256") == hashlib.sha256(stopped_bytes).hexdigest()
                    and journal.get("old_directory_device") == old_directory.st_dev
                    and journal.get("old_directory_inode") == old_directory.st_ino,
                    "journal hashes or directory anchors do not cover the actual producer bytes",
                )
                report.require(
                    label + "/fresh-candidate",
                    candidate.get("token") == journal.get("new_token") != old_token
                    and candidate.get("pane_id") == "w1:p2"
                    and candidate.get("terminal_id") == "term-2"
                    and _state(root).get("revive_launch_count") == 2
                    and new_process.poll() is None,
                    "recovery reused the old generation or failed to pin a separate replacement",
                )
                if phase == "ready":
                    report.require(label + "/old-preserved-before-publication",
                                   record_path.read_bytes() == old_bytes and not archive.exists()
                                   and _revive_tree(queue) == old_queue,
                                   "producer retired the old generation before its candidate was publishable")
                else:
                    report.require(label + "/old-archived-before-cleanup",
                                   archive.is_dir() and not (operation / "new").exists(),
                                   "published interruption did not preserve both generation directories")
                    new_process.kill()
                    new_process.wait(timeout=5)
                    report.require(label + "/published-candidate-dead", new_process.returncode == -9,
                                   "the published replacement was not actually killed before cleanup")
                _revive_dry_run(harness, report, root, consumer, label + "/pending-dry-run",
                                batch=False, action="recover")
                recovery_arguments = (("revive", "--all", *common) if phase == "published"
                                      else ("revive", "worker", "--expected-token", old_token, *common))
                recovered = harness._invoke_one(consumer, root, recovery_arguments)
                try:
                    result: object = json.loads(recovered.stdout)
                except ValueError:
                    result = None
                if phase == "published":
                    report.require(
                        label + "/batch-completion",
                        isinstance(result, dict) and result.get("dry_run") is False
                        and result.get("revived") == 1 and result.get("blocked") == 0,
                        f"batch did not complete the pending old generation: {recovered!r}",
                    )
                    agents = result.get("agents") if isinstance(result, dict) else None
                    result = agents[0] if isinstance(agents, list) and len(agents) == 1 else None
                report.require(
                    label + "/cross-edition-completion",
                    recovered.returncode == 0 and isinstance(result, dict)
                    and result.get("revived") is True and result.get("previous_token") == old_token
                    and result.get("tab_closed") is True,
                    f"other edition could not complete the saved transaction: {recovered!r}",
                )
                final_state = _state(root)
                launches = final_state.get("revive_launch_arguments")
                report.require(
                    label + "/no-relaunch-or-input",
                    final_state.get("revive_launch_count") == 2
                    and isinstance(launches, list) and len(launches) == 2
                    and launches[-1] == expected_arguments
                    and not final_state.get("revive_input_calls") and not final_state.get("submitted"),
                    "recovery relaunched a candidate, changed its recorded policy or replayed old work",
                )
                report.require(
                    label + "/exact-stale-pane-cleanup",
                    final_state.get("closed_panes") == ["w1:p1"]
                    and not journal_path.exists() and not (operation / "new").exists(),
                    "cleanup closed another generation or retained a completed journal",
                )
                report.require(
                    label + "/complete-archive",
                    archive.is_dir() and (archive / "agent.json").read_bytes() == stopped_bytes
                    and _revive_tree(archive / "queue") == old_queue
                    and (archive / "output.json").is_file()
                    and _revive_document(archive / "output.json").get("text") == "preserved old terminal output\n",
                    "archive omitted or rewrote producer bytes, output, queue, binding or quarantine artifacts",
                )
                report.require(
                    label + "/published-byte-and-task-preservation",
                    record_path.read_bytes() == ready_bytes
                    and candidate.get("profile") == "recovery" and candidate.get("model") == "recovery-model"
                    and candidate.get("cwd") == str(root) and candidate.get("resume") == "session-1"
                    and candidate.get("goal") == "finish the recorded task" and candidate.get("paused") is True
                    and candidate.get("revived_from") == old_token
                    and not (record_path.parent / "queue").exists(),
                    "published candidate bytes, saved task, policy or absence of a new queue changed",
                )
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)


def _bootstrap(harness: Harness, report: Report) -> None:
    case = harness.case("bootstrap")
    python, rust = harness.invoke(case, ("--version",))
    report.exact("bootstrap/version", python, rust, 0)
    python, rust = harness.invoke(case, ("--userguide",))
    report.exact("bootstrap/userguide", python, rust, 0)
    report.require(
        "bootstrap/userguide-shape",
        "possibly_submitted" in python.stdout and "inflight" in python.stdout,
        f"installed guide omitted recovery contract: {python!r}",
    )
    help_python, help_rust = harness.invoke(case, ("--help",))
    required = (
        "send",
        "drain",
        "status",
        "read",
        "--pane",
        "--session",
        "--queue",
        "--ready-timeout",
        "--working-timeout",
        "--herdr-bin",
    )
    for edition, outcome in (("python", help_python), ("rust", help_rust)):
        report.require(
            f"bootstrap/help/{edition}",
            outcome.returncode == 0
            and outcome.stderr == ""
            and all(value in outcome.stdout for value in required),
            f"help schema incomplete: {outcome!r}",
        )
    for edition, outcome in zip(
        ("python", "rust"), harness.invoke(case, ()), strict=True
    ):
        report.require(
            f"bootstrap/bare/{edition}",
            outcome.returncode == 0 and "send" in outcome.stdout and outcome.stderr == "",
            f"bare invocation is not an orientation: {outcome!r}",
        )


def _status_and_read(harness: Harness, report: Report) -> None:
    case = harness.case("status")
    common = (
        "--herdr-bin",
        "<HERDR>",
        "--pane",
        "w1:p1",
        "--agent",
        "codex",
        "--workspace",
        "project",
        "--cwd",
        "<ROOT>",
    )
    python, rust = harness.invoke(case, ("status", *common, "--queue", "<ROOT>/queue"))
    report.exact("status/exact-pane", python, rust, 0)
    report.require(
        "status/observational",
        not (case.python_root / "queue").exists() and not (case.rust_root / "queue").exists(),
        "status created an absent queue",
    )

    python, rust = harness.invoke(
        case,
        (
            "status",
            "--herdr-bin",
            "<HERDR>",
            "--session-agent",
            "codex",
            "--session",
            "session-1",
            "--queue",
            "<ROOT>/queue",
        ),
    )
    report.exact("status/stable-session", python, rust, 0)

    for root in (case.python_root, case.rust_root):
        state = _state(root)
        state["read_unwrapped"] = ""
        state["read_recent"] = "fallback transcript\n"
        (root / "state.json").write_text(json.dumps(state, sort_keys=True), encoding="utf-8")
    python, rust = harness.invoke(case, ("read", *common, "--lines", "17"))
    report.exact("read/fallback", python, rust, 0)
    report.require(
        "read/fallback-shape",
        python.stdout == "fallback transcript\n",
        f"unexpected transcript: {python!r}",
    )


def _successful_send(harness: Harness, report: Report) -> None:
    case = harness.case("successful-send")
    text = "first line\nsecond 'quoted' line\nthird line"
    success_arguments = (
        "send",
        text,
        "--herdr-bin",
        "<HERDR>",
        "--pane",
        "w1:p1",
        "--queue",
        "<ROOT>/queue",
    )
    python, rust = harness.invoke(case, success_arguments)
    report.exact("send/success", python, rust, 0)
    report.require(
        "send/literal-atomic-text",
        _state(case.python_root).get("submitted") == [text]
        and _state(case.rust_root).get("submitted") == [text],
        "one implementation changed or split the submitted text",
    )
    report.require(
        "send/durable-state",
        _queue_snapshot(case.python_root, "queue") == _queue_snapshot(case.rust_root, "queue"),
        "processed queue artifacts differ",
    )


def _pending_and_ambiguous(harness: Harness, report: Report) -> None:
    busy = harness.case("pending", {"status": "working"})
    pending_arguments = (
        "send",
        "keep pending",
        "--herdr-bin",
        "<HERDR>",
        "--pane",
        "w1:p1",
        "--queue",
        "<ROOT>/queue",
        "--ready-timeout",
        "0",
    )
    python, rust = harness.invoke(busy, pending_arguments)
    report.exact("send/pending", python, rust, 75)
    report.require(
        "send/pending-state",
        _queue_snapshot(busy.python_root, "queue")
        == _queue_snapshot(busy.rust_root, "queue"),
        "pending queue artifacts differ",
    )
    report.require(
        "send/pending-not-injected",
        _state(busy.python_root).get("submitted") == []
        and _state(busy.rust_root).get("submitted") == [],
        "busy prompt was injected",
    )

    ambiguous = harness.case("ambiguous", {"run_mode": "fail"})
    ambiguous_arguments = (
        "send",
        "only once",
        "--herdr-bin",
        "<HERDR>",
        "--pane",
        "w1:p1",
        "--queue",
        "<ROOT>/queue",
    )
    python, rust = harness.invoke(ambiguous, ambiguous_arguments)
    report.exact("send/possibly-submitted", python, rust, 76)
    report.require(
        "send/possibly-submitted-once",
        _state(ambiguous.python_root).get("submitted") == ["only once"]
        and _state(ambiguous.rust_root).get("submitted") == ["only once"],
        "ambiguous prompt was not attempted exactly once",
    )
    report.require(
        "send/ambiguous-state",
        _queue_snapshot(ambiguous.python_root, "queue")
        == _queue_snapshot(ambiguous.rust_root, "queue"),
        "failed queue artifacts differ",
    )


def _adversarial_queue(harness: Harness, report: Report) -> None:
    case = harness.case("malformed-head")
    for root in (case.python_root, case.rust_root):
        inbox = root / "queue" / "inbox"
        inbox.mkdir(parents=True, mode=0o700)
        (inbox / "000000000001.json").write_bytes(b"{not json\n")
        (inbox / "000000000002.json").write_text(
            '{"id":"good","text":"deliver after poison"}\n', encoding="utf-8"
        )
    arguments = (
        "drain",
        "--herdr-bin",
        "<HERDR>",
        "--pane",
        "w1:p1",
        "--queue",
        "<ROOT>/queue",
    )
    python, rust = harness.invoke(case, arguments)
    report.exact("queue/malformed-head", python, rust, 76)
    report.require(
        "queue/malformed-raw-preserved",
        (case.python_root / "queue/failed/000000000001.json").read_bytes()
        == (case.rust_root / "queue/failed/000000000001.json").read_bytes()
        == b"{not json\n",
        "malformed bytes were changed",
    )

    contradiction = harness.case("contradictory-target")
    python, rust = harness.invoke(
        contradiction,
        (
            "status",
            "--herdr-bin",
            "<HERDR>",
            "--pane",
            "wrong:pane",
            "--session-agent",
            "codex",
            "--session",
            "session-1",
        ),
    )
    report.require(
        "target/pane-session-contradiction",
        python.returncode == rust.returncode == 75
        and "expected exact pane" in python.stderr
        and "expected exact pane" in rust.stderr,
        f"contradictory identity did not fail closed: python={python!r} rust={rust!r}",
    )

    for label, payload in (
        ("nonfinite-json", b'{"id":"bad","text":NaN}\n'),
        ("boolean-attempts", b'{"id":"bad","text":"prompt","delivery_attempts":true}\n'),
        ("empty-text", b'{"id":"bad","text":""}\n'),
    ):
        invalid = harness.case(label)
        for root in (invalid.python_root, invalid.rust_root):
            inbox = root / "queue/inbox"
            inbox.mkdir(parents=True, mode=0o700)
            (inbox / "bad.json").write_bytes(payload)
        python, rust = harness.invoke(
            invalid,
            (
                "drain",
                "--herdr-bin",
                "<HERDR>",
                "--pane",
                "w1:p1",
                "--queue",
                "<ROOT>/queue",
            ),
        )
        report.exact(f"queue/{label}", python, rust, 76)

    exhausted = harness.case("exhausted-attempts")
    for root in (exhausted.python_root, exhausted.rust_root):
        inbox = root / "queue/inbox"
        inbox.mkdir(parents=True, mode=0o700)
        (inbox / "exhausted.json").write_text(
            '{"id":"exhausted","text":"keep me","delivery_attempts":1}\n',
            encoding="utf-8",
        )
    python, rust = harness.invoke(
        exhausted,
        (
            "drain",
            "--herdr-bin",
            "<HERDR>",
            "--pane",
            "w1:p1",
            "--queue",
            "<ROOT>/queue",
            "--max-attempts",
            "1",
        ),
    )
    report.exact("queue/exhausted-is-pending", python, rust, 75)

    special = harness.case("fifo-artifact")
    for root in (special.python_root, special.rust_root):
        inbox = root / "queue/inbox"
        inbox.mkdir(parents=True, mode=0o700)
        os.mkfifo(inbox / "pipe.json", mode=0o600)
    python, rust = harness.invoke(
        special,
        (
            "drain",
            "--herdr-bin",
            "<HERDR>",
            "--pane",
            "w1:p1",
            "--queue",
            "<ROOT>/queue",
        ),
    )
    report.exact("queue/fifo-does-not-block", python, rust, 76)

    missing = harness.case("missing-target")
    python, rust = harness.invoke(
        missing,
        ("send", "do not strand", "--herdr-bin", "<HERDR>", "--queue", "<ROOT>/queue"),
    )
    report.require(
        "target/missing-no-mutation",
        python.returncode == rust.returncode == 75
        and not (missing.python_root / "queue").exists()
        and not (missing.rust_root / "queue").exists(),
        f"missing target created durable state: python={python!r} rust={rust!r}",
    )

    wrong_workspace = harness.case("wrong-workspace-response", {"workspace_response_id": "w9"})
    python, rust = harness.invoke(
        wrong_workspace,
        (
            "status",
            "--herdr-bin",
            "<HERDR>",
            "--pane",
            "w1:p1",
            "--workspace",
            "project",
        ),
    )
    report.require(
        "target/workspace-response-id",
        python.returncode == rust.returncode == 69
        and "returned workspace" in python.stderr
        and "returned workspace" in rust.stderr,
        f"wrong workspace object was accepted: python={python!r} rust={rust!r}",
    )


def _shared_queue_interop(harness: Harness, report: Report) -> None:
    """Each edition must consume the other edition's binding and pending artifact."""

    for label, producer, consumer in (
        ("python-to-rust", harness.python, harness.rust),
        ("rust-to-python", harness.rust, harness.python),
    ):
        case = harness.case(f"interop-{label}", {"status": "working"})
        root = case.python_root
        pending = harness._invoke_one(
            producer,
            root,
            (
                "send",
                "shared queue prompt",
                "--herdr-bin",
                "<HERDR>",
                "--pane",
                "w1:p1",
                "--queue",
                "<ROOT>/queue",
                "--ready-timeout",
                "0",
            ),
        )
        state = _state(root)
        state["status"] = "idle"
        (root / "state.json").write_text(json.dumps(state, sort_keys=True), encoding="utf-8")
        drained = harness._invoke_one(
            consumer,
            root,
            (
                "drain",
                "--herdr-bin",
                "<HERDR>",
                "--pane",
                "w1:p1",
                "--queue",
                "<ROOT>/queue",
            ),
        )
        binding: object = json.loads((root / "queue/target.json").read_text(encoding="utf-8"))
        report.require(
            f"interop/{label}",
            pending.returncode == 75
            and drained.returncode == 0
            and _state(root).get("submitted") == ["shared queue prompt"]
            and isinstance(binding, dict)
            and binding.get("pane_id") == "w1:p1",
            f"shared queue was not interoperable: pending={pending!r} drained={drained!r}",
        )


def _cross_process_serialization(harness: Harness, report: Report) -> None:
    """Mixed target forms and different TMPDIR values still serialize one resolved pane."""

    case = harness.case("cross-process-lock", {"run_mode": "gate"})
    root = case.python_root

    def launch(command: Sequence[str], arguments: Sequence[str], tmpdir: Path) -> subprocess.Popen[str]:
        tmpdir.mkdir(mode=0o700)
        expanded = [
            value.replace("<ROOT>", str(root)).replace("<HERDR>", str(root / "fake-herdr"))
            for value in arguments
        ]
        refusal = Harness.refuse_host_cli(root, expanded)
        if refusal is not None:
            raise RuntimeError(f"cross harness host CLI guard: {refusal}")
        environment = with_case_git(without_ambient_executables(dict(os.environ)), root)
        existing = environment.get("PYTHONPATH", "")
        local = str(REPO_ROOT / "py")
        environment["PYTHONPATH"] = local if not existing else local + os.pathsep + existing
        environment.update(
            {"LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "NO_COLOR": "1", "TMPDIR": str(tmpdir)}
        )
        environment["PATH"] = guarded_path(root, environment.get("PATH"))
        return subprocess.Popen(
            [*command, *expanded],
            cwd=root,
            env=environment,
            stdin=subprocess.DEVNULL,  # as in Harness._invoke_one
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )

    first = launch(
        harness.python,
        (
            "send",
            "from pane",
            "--herdr-bin",
            "<HERDR>",
            "--pane",
            "w1:p1",
            "--queue",
            "<ROOT>/pane-queue",
        ),
        root / "tmp-a",
    )
    second: subprocess.Popen[str] | None = None
    try:
        deadline = time.monotonic() + 5
        while not (root / "run-entered").exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        second = launch(
            harness.rust,
            (
                "send",
                "from session",
                "--herdr-bin",
                "<HERDR>",
                "--session-agent",
                "codex",
                "--session",
                "session-1",
                "--queue",
                "<ROOT>/session-queue",
            ),
            root / "tmp-b",
        )
        time.sleep(0.25)
        serialized = _state(root).get("submitted") == ["from pane"]
        (root / "run-release").write_text("release\n", encoding="utf-8")
        first_out, first_err = first.communicate(timeout=10)
        second_out, second_err = second.communicate(timeout=10)
        report.require(
            "interop/cross-process-canonical-lock",
            serialized and first.returncode == second.returncode == 0,
            "mixed target forms overlapped: "
            f"state={_state(root)!r} first=({first.returncode},{first_out!r},{first_err!r}) "
            f"second=({second.returncode},{second_out!r},{second_err!r})",
        )
    finally:
        (root / "run-release").touch()
        for process in (first, second):
            if process is not None and process.poll() is None:
                process.kill()
                process.communicate()


def _invalid_cli(harness: Harness, report: Report) -> None:
    case = harness.case("invalid-cli")
    for label, arguments, expected_code in (
        ("unknown-command", ("unknown", *FIXTURE_HERDR), 2),
        ("nan-timeout", ("status", "--ready-timeout", "nan", *FIXTURE_HERDR), 2),
        ("missing-message", ("send", "--herdr-bin", "<HERDR>", "--pane", "w1:p1"), 2),
        ("abbreviated-option", ("status", "--pan", "w1:p1", *FIXTURE_HERDR), 2),
        ("extra-userguide-positionals", ("userguide", "one", "two"), 2),
        ("userguide-invalid-command", ("--userguide", "unknown"), 2),
        ("option-value-stolen-by-help", ("status", "--pane", "--help", *FIXTURE_HERDR), 2),
        ("option-value-stolen-by-version", ("status", "--pane", "--version", *FIXTURE_HERDR), 2),
        ("option-value-stolen-by-option", ("status", "--pane", "--queue", "state", *FIXTURE_HERDR), 2),
        ("attempts-over-bound", ("status", "--max-attempts", "1000001", *FIXTURE_HERDR), 2),
        ("attempts-underscore", ("status", "--max-attempts", "1_0", *FIXTURE_HERDR), 2),
        ("attempts-unicode", ("status", "--max-attempts", "١٢", *FIXTURE_HERDR), 2),
        ("lines-over-bound", ("read", "--lines", "1000001", *FIXTURE_HERDR), 2),
        ("timeout-underscore", ("status", "--ready-timeout", "1_0", *FIXTURE_HERDR), 2),
        ("timeout-unicode", ("status", "--ready-timeout", "١.0", *FIXTURE_HERDR), 2),
    ):
        python, rust = harness.invoke(case, arguments)
        report.require(
            f"cli/{label}",
            python.returncode == rust.returncode == expected_code
            and "traceback" not in (python.stderr + rust.stderr).lower()
            and "panicked" not in (python.stderr + rust.stderr).lower(),
            f"invalid invocation was not a clean usage error: python={python!r} rust={rust!r}",
        )

    python, rust = harness.invoke(case, ("status", "--pane", "", *FIXTURE_HERDR))
    expected = "target needs --pane or a stable session value"
    report.require(
        "cli/empty-exact-pane",
        python.returncode == rust.returncode == 75
        and python.stdout == rust.stdout == ""
        and expected in python.stderr
        and expected in rust.stderr,
        "empty exact pane did not fail target validation before Herdr resolution: "
        f"python={python!r} rust={rust!r}",
    )

    registry = "<ROOT>/legacy state's $literal; directory"
    common = (f"--registry={registry}", "--herdr-bin=<HERDR>")
    for label, arguments in (
        ("missing-name", (*common, "stop")),
        ("unknown-option", (*common, "stop", "worker", "--unrecognized")),
        ("missing-option-value", (*common, "stop", "worker", "--expected-token")),
    ):
        outcomes = harness.invoke(case, arguments)
        report.require(
            f"cli/stop-refusal/{label}",
            all(
                outcome.returncode == 2
                and (recovery := _stop_recovery_arguments(outcome)) is not None
                and recovery[-1] == "doctor"
                and _option_values(recovery, "--registry") == [registry]
                and _option_values(recovery, "--herdr-bin") == ["<ROOT>/fake-herdr"]
                for outcome in outcomes
            ),
            f"legacy syntax refusal lost its reason or selected globals: {outcomes!r}",
        )
    outcomes = harness.invoke(case, (
        "--registry=stop", "--herdr-bin=<HERDR>", "status", "--unrecognized",
    ))
    report.require(
        "cli/non-stop-literal-value",
        all(outcome.returncode == 2 and "Recovery command:" not in outcome.stderr
            for outcome in outcomes),
        f"a literal option value invented stop recovery context: {outcomes!r}",
    )
    report.require(
        "cli/stop-refusal/no-registry-side-effect",
        all(not (root / registry.removeprefix("<ROOT>/")).exists()
            for root in (case.python_root, case.rust_root)),
        "legacy syntax refusal created persistent registry state",
    )


def _managed_lifecycle(harness: Harness, report: Report) -> None:
    """Exercise the same named lifecycle and cross-edition durable registry format."""
    def normalized(outcome: Outcome) -> object:
        if outcome.returncode != 0:
            return outcome
        value: object = json.loads(outcome.stdout)
        def clean(item: object) -> object:
            if isinstance(item, list):
                return [clean(entry) for entry in item]
            if isinstance(item, dict):
                return {
                    str(key): ("<TOKEN>" if key == "token" else 0 if key == "created_at"
                               else "<ARCHIVE>" if key == "archive"
                               # Each edition's fixture runs its own harness process: only
                               # its pid and start time differ.
                               else {**child, "pid": "<PID>", "starttime_ticks": "<TICKS>"}
                               if key == "harness_identity" and isinstance(child, dict)
                               else clean(child))
                    for key, child in item.items()
                }
            return item
        return clean(value)

    for kind in ("codex", "claude"):
        case = harness.case(f"managed-{kind}")
        common: tuple[str, ...] = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry", "--goal-command-json", '["<HERDR>","goal-rpc"]')
        start = ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
                 "--harness", kind, "--model", "selected-model", "--harness-arg=--extra",
                 *(("--resume", "session-1") if kind == "claude" else ()), *common)
        python, rust = harness.invoke(case, start)
        report.require(f"managed/{kind}/start", python.returncode == rust.returncode == 0 and normalized(python) == normalized(rust),
                       f"start diverged: {python!r} {rust!r}")
        if python.returncode or rust.returncode:
            continue
        for command in (("list",), ("status", "--name", "worker"), ("bind-session", "session-1", "--name", "worker"), ("wait", "--name", "worker", "--ready-timeout", "0"),
                        ("goal", "finish the task", "--name", "worker"), ("goal", "--name", "worker"),
                        ("send", "literal\nmessage", "--name", "worker")):
            python, rust = harness.invoke(case, (*command, *common))
            report.require(f"managed/{kind}/{command[0]}", python.returncode == rust.returncode == 0 and normalized(python) == normalized(rust),
                           f"managed command diverged: {python!r} {rust!r}")
        python, rust = harness.invoke(case, ("read", "--name", "worker", *common))
        report.exact(f"managed/{kind}/read", python, rust, 0)
        python, rust = harness.invoke(case, ("stop", "worker", *common))
        report.require(f"managed/{kind}/stop", python.returncode == rust.returncode == 0 and normalized(python) == normalized(rust),
                       f"stop diverged: {python!r} {rust!r}")
        report.require(f"managed/{kind}/literal-launch", _state(case.python_root).get("launch_arguments") == _state(case.rust_root).get("launch_arguments"),
                       "harness arguments differed")

    compatibility_cases = (
        (
            "claude-unstructured-effort",
            ("--harness", "claude", "--harness-arg=--effort=high"),
            ["--effort=high"],
        ),
        (
            "codex-nonoverriding-config",
            (
                "--harness", "codex", "--model", "selected-model",
                "--harness-arg=-c", "--harness-arg=sandbox_mode=read-only",
            ),
            [
                "--no-alt-screen", "--model", "selected-model",
                "-c", "sandbox_mode=read-only",
            ],
        ),
    )
    for label, launch_policy, expected_arguments in compatibility_cases:
        case = harness.case(f"managed-compatibility-{label}")
        common = (
            "--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry",
            "--goal-command-json", '["<HERDR>","goal-rpc"]',
        )
        outcomes = harness.invoke(case, (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            *launch_policy, *common,
        ))
        report.require(
            f"managed/compatibility/{label}",
            all(outcome.returncode == 0 for outcome in outcomes)
            and all(
                (cast(list[object], _state(root).get("launch_arguments"))[2:] if label == "claude-unstructured-effort"
                 else _state(root).get("launch_arguments")) == expected_arguments
                for root in (case.python_root, case.rust_root)
            ),
            f"legacy raw launch policy regressed: {outcomes!r}",
        )

    profile_selectors = (
        ("split-config-profile", ("--harness-arg=-c", "--harness-arg=profile=attacker")),
        ("attached-config-profile", ("--harness-arg=--config=profile=attacker",)),
        ("quoted-config-profile", ('--harness-arg=-c"profile"="attacker"',)),
    )
    for label, raw_arguments in profile_selectors:
        case = harness.case(f"managed-profile-selector-refusal-{label}")
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
        outcomes = harness.invoke(case, (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--harness", "codex", "--model", "structured",
            *raw_arguments, *common,
        ))
        report.require(
            f"managed/profile-selector-refusal/{label}",
            all(
                outcome.returncode == 75 and "raw Codex profile" in outcome.stderr
                for outcome in outcomes
            )
            and all(
                not (root / "registry").exists()
                for root in (case.python_root, case.rust_root)
            ),
            f"indirect Codex profile selector was accepted or allocated state: {outcomes!r}",
        )

    # A record written by Python can be read, messaged and archived by Rust, and vice versa.
    for label, producer, consumer in (("python-rust", harness.python, harness.rust), ("rust-python", harness.rust, harness.python)):
        case = harness.case(f"managed-interop-{label}")
        root = case.python_root
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
        started = harness._invoke_one(producer, root, ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common))
        sent = harness._invoke_one(consumer, root, ("send", "shared registry", "--name", "worker", *common))
        stopped = harness._invoke_one(consumer, root, ("stop", "worker", *common))
        report.require(f"managed/interop/{label}", started.returncode == sent.returncode == stopped.returncode == 0 and _state(root).get("submitted") == ["shared registry"],
                       f"registry interop failed: {started!r} {sent!r} {stopped!r}")

    for label, change in (("offline", {"offline": True}), ("busy", {"status": "working"}), ("extra-pane", {"extra_pane": True})):
        case = harness.case(f"managed-{label}")
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
        harness.invoke(case, ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common))
        for root in (case.python_root, case.rust_root):
            state = _state(root)
            state.update(change)
            (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
        if label == "extra-pane":
            python, rust = harness.invoke(case, ("stop", "worker", *common))
            report.require("managed/stop-extra-pane", python.returncode == rust.returncode == 75
                           and "ownership changed" in python.stderr and "ownership changed" in rust.stderr,
                           f"unsafe tab close was accepted: {python!r} {rust!r}")
        else:
            python, rust = harness.invoke(case, ("send", "retain this prompt", "--name", "worker", "--ready-timeout", "0", *common))
            report.require(f"managed/{label}-pending", python.returncode == rust.returncode == 75
                           and '"outcome": "pending"' in python.stdout and '"outcome": "pending"' in rust.stdout,
                           f"prompt was not durably retained: {python!r} {rust!r}")
            report.require(f"managed/{label}-durable-state", _queue_snapshot(case.python_root, "registry/worker/queue") == _queue_snapshot(case.rust_root, "registry/worker/queue"),
                           "pending managed queue artifacts differed")
        report.require(f"managed/{label}-no-mutation", _state(case.python_root).get("submitted") == []
                       and _state(case.rust_root).get("submitted") == []
                       and not _state(case.python_root).get("closed") and not _state(case.rust_root).get("closed"),
                       "failure injected a prompt or closed a tab")

    case = harness.case("managed-failed-launch", {"start_failure": True})
    common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
    python, rust = harness.invoke(case, ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common))
    report.require("managed/failed-launch-retained", python.returncode == rust.returncode == 75
                   and (case.python_root / "registry/worker/agent.json").exists()
                   and (case.rust_root / "registry/worker/agent.json").exists()
                   and not _state(case.python_root).get("closed") and not _state(case.rust_root).get("closed"),
                   f"launch failed without inspectable state: {python!r} {rust!r}")
    for root in (case.python_root, case.rust_root):
        state = _state(root)
        state["name"] = "replacement"
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
    python, rust = harness.invoke(case, ("stop", "worker", *common))
    report.require("managed/failed-launch-replacement-preserved", python.returncode == rust.returncode == 69
                   and not _state(case.python_root).get("closed") and not _state(case.rust_root).get("closed"),
                   f"cleanup targeted replacement harness: {python!r} {rust!r}")

    for correct in (True, False):
        objective = "finish this task" if correct else "different objective"
        screen = f"Replace goal?\nNew objective: {objective}\n› 1. Replace current goal  Set the new objective and start it now\n2. Cancel  Keep the current goal\nPress enter to confirm or esc to go back"
        case = harness.case(f"goal-menu-{correct}", {"goal_menu": True, "screen": screen})
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry", "--goal-command-json", '["<HERDR>","goal-rpc"]')
        harness.invoke(case, ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common))
        python, rust = harness.invoke(case, ("goal", "finish this task", "--name", "worker", *common))
        expected = 0 if correct else 76
        report.require(f"managed/goal-replacement-{correct}", python.returncode == rust.returncode == expected
                       and bool(_state(case.python_root).get("confirmed_goal")) == correct
                       and bool(_state(case.rust_root).get("confirmed_goal")) == correct,
                       f"goal menu handling diverged: {python!r} {rust!r}")

    for label, producer, consumer in (("python-rust", harness.python, harness.rust), ("rust-python", harness.rust, harness.python)):
        case = harness.case(f"queued-goal-interop-{label}")
        root = case.python_root
        common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry", "--goal-command-json", '["<HERDR>","goal-rpc"]')
        started = harness._invoke_one(producer, root, ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common))
        state = _state(root)
        state["status"] = "working"
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
        queued = harness._invoke_one(producer, root, ("goal", "finish this task", "--name", "worker", "--ready-timeout", "0", *common))
        state = _state(root)
        state.update({"status": "idle", "goal_menu": True,
                      "screen": "Replace goal?\nNew objective: finish this task\n› 1. Replace current goal  Set the new objective and start it now\n2. Cancel  Keep the current goal\nPress enter to confirm or esc to go back"})
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
        drained = harness._invoke_one(consumer, root, ("drain", "--name", "worker", *common))
        goal = harness._invoke_one(consumer, root, ("goal", "--name", "worker", *common))
        report.require(f"managed/queued-goal/{label}", started.returncode == drained.returncode == goal.returncode == 0
                       and queued.returncode == 75 and bool(_state(root).get("confirmed_goal"))
                       and _state(root).get("submitted") == ["/goal finish this task"]
                       and json.loads(goal.stdout).get("delivery") == "delivered",
                       f"queued goal lost durable operation identity: {started!r} {queued!r} {drained!r} {goal!r}")
        state = _state(root)
        state["confirmed_goal"] = False
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
        raw = harness._invoke_one(consumer, root, ("send", "/goal finish this task", "--name", "worker", *common))
        report.require(f"managed/raw-goal-not-authorized/{label}", raw.returncode == 76 and not _state(root).get("confirmed_goal"),
                       f"ordinary text incorrectly inherited goal confirmation: {raw!r}")


def build_report(python_command: Sequence[str], rust_command: Sequence[str]) -> Report:
    """Run the complete black-box differential."""

    report = Report()
    with tempfile.TemporaryDirectory(prefix="herdr-agent-cross-") as temporary:
        harness = Harness(Path(temporary), python_command, rust_command)
        try:
            _bootstrap(harness, report)
            _status_and_read(harness, report)
            _successful_send(harness, report)
            _pending_and_ambiguous(harness, report)
            _adversarial_queue(harness, report)
            _shared_queue_interop(harness, report)
            _cross_process_serialization(harness, report)
            _invalid_cli(harness, report)
            _managed_lifecycle(harness, report)
            harness.require_no_host_cli(report)
        finally:
            harness.close()
    return report


def compare_herdr_agent(python_command: Sequence[str], rust_command: Sequence[str]) -> int:
    """Print the paired report and return a conventional status."""

    report = build_report(python_command, rust_command)
    if report.failures:
        for failure in report.failures:
            print(f"DIVERGENCE [{failure}]")
        print(
            f"cross[herdr-agent]: {len(report.failures)} divergence(s) "
            f"out of {report.checks} paired checks"
        )
        return 1
    print(f"cross[herdr-agent]: OK - {report.checks} paired checks agree")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Standalone source-tree entry point used for focused development."""

    del argv
    python = [sys.executable, "-m", "agentctl.legacy_cli"]
    launcher = REPO_ROOT / "rs/bin/herdr-agent"
    environment = dict(os.environ)
    environment["AGENT_UTILS_RS_ENSURE_ONLY"] = "1"
    ensured = subprocess.run(
        [str(launcher)],
        cwd=REPO_ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
        timeout=300,
    )
    if ensured.returncode != 0 or not ensured.stdout.strip():
        print(f"cannot resolve Rust herdr-agent: {ensured.stderr}", file=sys.stderr)
        return 1
    rust = [ensured.stdout.strip()]
    return compare_herdr_agent(python, rust)


if __name__ == "__main__":
    raise SystemExit(main())

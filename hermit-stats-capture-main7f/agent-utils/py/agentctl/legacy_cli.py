#!/usr/bin/env python3
"""Command line for durable interactive-agent messaging through Herdr.

This module can also run directly from a source checkout. Insert the package parent before package
imports so that direct execution behaves like the installed console command.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
from collections.abc import Sequence

_PKG_PARENT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if _PKG_PARENT not in sys.path:
    sys.path.insert(0, _PKG_PARENT)

from agentctl import __version__
from agentctl.agent import Target, drain, read, send, status
from agentctl.client import HerdrClient
from agentctl.errors import AgentPending, AgentPossiblySubmitted, HerdrRunError
from agentctl.subagents import ManagedAgents
from agentctl.stop_recovery import StopRecoveryParser, stop_refusal_message

_MAX_WAIT_SECONDS = 31_536_000.0
_MAX_COUNT = 1_000_000
_ASCII_FLOAT = re.compile(
    r"[+-]?(?:(?:[0-9]+(?:\.[0-9]*)?)|(?:\.[0-9]+))(?:[eE][+-]?[0-9]+)?\Z"
)
_ASCII_UINT = re.compile(r"[0-9]+\Z")


def _ascii_float(value: str) -> float:
    if _ASCII_FLOAT.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("expected an ASCII decimal number")
    return float(value)


def _bounded_uint(value: str) -> int:
    if _ASCII_UINT.fullmatch(value) is None:
        raise argparse.ArgumentTypeError("expected an ASCII unsigned integer")
    parsed = int(value, 10)
    if parsed > _MAX_COUNT:
        raise argparse.ArgumentTypeError(f"value must not exceed {_MAX_COUNT}")
    return parsed


def _parser() -> argparse.ArgumentParser:
    parser = StopRecoveryParser(
        prog="herdr-agent",
        description="Queue, submit, inspect, and read interactive agents hosted in Herdr panes.",
        allow_abbrev=False,
    )
    parser.add_argument("--version", action="version", version=f"herdr-agent {__version__}")
    parser.add_argument("--userguide", action="store_true", help="print the installed user guide and exit")
    parser.add_argument("command", nargs="?", choices=("start", "stop", "list", "wait", "goal", "bind-session", "send", "drain", "status", "read", "userguide"))
    parser.add_argument("text", nargs="?")
    parser.add_argument("--file", help="read a message from this artifact")
    parser.add_argument("--pane")
    parser.add_argument("--session-agent")
    parser.add_argument("--session")
    parser.add_argument("--agent", dest="expected_agent")
    parser.add_argument("--workspace", dest="expected_workspace")
    parser.add_argument("--cwd", dest="expected_cwd")
    parser.add_argument("--queue", default=".herdr-agent")
    parser.add_argument("--name", help="target a registered, named interactive subagent")
    parser.add_argument("--registry", default=".herdr-agents", help="private managed-agent state directory")
    parser.add_argument("--workspace-id", help="start in this exact Herdr workspace (default: current or subagents)")
    parser.add_argument("--harness", default="codex", help="Herdr agent kind; codex and claude have model/resume presets")
    parser.add_argument("--model")
    parser.add_argument("--resume")
    parser.add_argument("--harness-arg", action="append", default=[], help="literal harness argument (repeat; use = for flags)")
    parser.add_argument("--brief", help="initial task after successful launch")
    parser.add_argument("--startup-timeout", type=_ascii_float, default=30.0)
    parser.add_argument("--goal-command-json", help="native Codex goal RPC argv as a JSON string array")
    parser.add_argument("--expected-token", help="exact current registry generation for stop; mandatory for adoption recovery")
    adoption_recovery = parser.add_mutually_exclusive_group()
    adoption_recovery.add_argument("--recover-legacy-adoption", action="store_true",
        help="stop only: recover an identity-less dead adopted row without changing its pane; requires token and raw digest")
    adoption_recovery.add_argument("--retire-dead-adoption", action="store_true",
        help="stop only: archive a running adopted record whose pinned harness is dead, without querying Herdr; requires token and raw digest")
    parser.add_argument("--expected-record-sha256",
        help="stop only: exact lowercase 64-hex SHA-256 of raw agent.json; requires an adoption recovery selector")
    parser.add_argument("--ready-timeout", type=_ascii_float, default=900.0)
    parser.add_argument("--working-timeout", type=_ascii_float, default=30.0)
    parser.add_argument("--max-attempts", type=_bounded_uint, default=3)
    parser.add_argument("--lines", type=_bounded_uint, default=500)
    parser.add_argument("--herdr-bin", default="herdr")
    return parser


def _managed(args: argparse.Namespace, client: HerdrClient) -> int:
    manager = ManagedAgents(client, args.registry)
    options: dict[str, object] = {
        "ready_timeout": args.ready_timeout, "working_timeout": args.working_timeout,
        "max_attempts": args.max_attempts,
    }
    name = args.name
    command = args.command
    if command != "stop" and (
        args.expected_token is not None or args.recover_legacy_adoption or args.retire_dead_adoption
        or args.expected_record_sha256 is not None
    ):
        raise ValueError("legacy recovery options are valid only with stop")
    goal_command: list[str] | None = None
    if args.goal_command_json is not None:
        decoded: object = json.loads(args.goal_command_json)
        if not isinstance(decoded, list) or not decoded or any(not isinstance(item, str) or not item for item in decoded):
            raise ValueError("goal command must be a nonempty JSON string array")
        goal_command = [str(item) for item in decoded]
    if command in ("start", "stop"):
        if args.text and name:
            raise ValueError("pass the agent name once, as a positional or --name")
        name = name or args.text
    if command != "list" and not name:
        raise ValueError(f"{command} needs a registered agent --name")
    if args.pane or args.session or args.session_agent or args.expected_agent or args.expected_workspace:
        raise ValueError("managed commands use registry identity; do not combine --name with pane/session assertions")
    if command == "start":
        if not args.expected_cwd:
            raise ValueError("start needs --cwd")
        if args.file is not None and args.brief is not None:
            raise ValueError("pass --brief or --file, not both")
        brief = args.brief
        if args.file is not None:
            with open(args.file, encoding="utf-8") as source:
                brief = source.read()
        result: object = manager.start(
            name, cwd=args.expected_cwd, workspace_id=args.workspace_id,
            harness=args.harness, model=args.model, resume=args.resume,
            harness_args=args.harness_arg, brief=brief,
            startup_timeout=args.startup_timeout, ready_timeout=args.ready_timeout,
            working_timeout=args.working_timeout, max_attempts=args.max_attempts,
        )
    elif command == "stop":
        recovery_selector = ("--retire-dead-adoption" if args.retire_dead_adoption
                             else "--recover-legacy-adoption" if args.recover_legacy_adoption
                             else None)
        if recovery_selector is not None and (
            args.expected_token is None or args.expected_record_sha256 is None
        ):
            raise ValueError(
                f"{recovery_selector} requires --expected-token and "
                "--expected-record-sha256"
            )
        if (recovery_selector is None
                and args.expected_record_sha256 is not None):
            raise ValueError(
                "--expected-record-sha256 requires --recover-legacy-adoption or --retire-dead-adoption"
            )
        result = manager.stop(
            name, expected_token=args.expected_token,
            recover_legacy_adoption=args.recover_legacy_adoption,
            retire_dead_adoption=args.retire_dead_adoption,
            expected_record_sha256=args.expected_record_sha256,
        )
    elif command == "list":
        if args.text or args.name:
            raise ValueError("list does not accept an agent name")
        result = manager.list()
    elif command == "send":
        result = manager.send(name, _message(args), message_id=None, expected_token=None, **options).__dict__
    elif command == "drain":
        drained = manager.drain(name, **options)
        result = drained.__dict__
    elif command == "status":
        result = manager.status(name)
    elif command == "read":
        sys.stdout.write(manager.read(name, lines=args.lines))
        return 0
    elif command == "wait":
        result = manager.wait(name, timeout=args.ready_timeout)
    elif command == "goal":
        goal = _message(args) if args.text is not None or args.file is not None else None
        result = manager.goal(name, goal, goal_command=goal_command, **options)
    elif command == "bind-session":
        if args.text is None or args.file is not None:
            raise ValueError("bind-session needs an explicit session id as its positional argument")
        result = manager.bind_session(name, args.text, goal_command=goal_command)
    else:
        raise ValueError(f"unsupported managed command: {command}")
    json.dump(result, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    if command == "drain":
        return 75 if drained.blocked else (76 if drained.quarantined else 0)
    return 0


def _target(args: argparse.Namespace) -> Target:
    return Target(
        pane_id=args.pane,
        session_agent=args.session_agent,
        session_value=args.session,
        expected_agent=args.expected_agent,
        expected_workspace=args.expected_workspace,
        expected_cwd=args.expected_cwd,
    )


def _message(args: argparse.Namespace) -> str:
    if args.file is not None:
        if args.text is not None:
            raise ValueError("pass message text or --file, not both")
        try:
            with open(args.file, encoding="utf-8") as handle:
                return handle.read()
        except (OSError, UnicodeError) as exc:
            raise ValueError(f"cannot read message artifact {args.file}: {exc}") from exc
    if args.text is None:
        raise ValueError("send needs message text or --file")
    return str(args.text)


def _guide() -> int:
    from importlib.resources import files

    try:
        text = (files("agentctl") / "AGENT_USER_GUIDE.md").read_text(encoding="utf-8")
    except (OSError, ModuleNotFoundError) as exc:
        print(f"herdr-agent: user guide unavailable: {exc}", file=sys.stderr)
        return 1
    sys.stdout.write(text)
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    """Run the durable interactive-agent messaging command-line interface."""

    parser = _parser()
    arguments = list(sys.argv[1:] if argv is None else argv)
    # Intermixed parsing reads options before positionals on some supported
    # Python versions. Use the same grammar first so an option error after a
    # recognized stop retains its parsed command and globals.
    parser.parse_known_args(arguments)
    args = parser.parse_intermixed_args(arguments)
    if args.userguide or args.command == "userguide":
        return _guide()
    if args.command is None:
        parser.print_help()
        return 0
    if (
        not math.isfinite(args.ready_timeout)
        or not math.isfinite(args.working_timeout)
        or args.ready_timeout < 0
        or args.working_timeout <= 0
        or args.ready_timeout > _MAX_WAIT_SECONDS
        or args.working_timeout > _MAX_WAIT_SECONDS
        or not math.isfinite(args.startup_timeout)
        or not 0 < args.startup_timeout <= 300
        or args.max_attempts <= 0
        or args.lines <= 0
    ):
        parser.error(
            "timeouts must be finite and positive (ready-timeout may be zero and must not exceed "
            f"{_MAX_WAIT_SECONDS:g}s); max-attempts and lines must be positive"
        )
    try:
        if args.retire_dead_adoption and args.command != "stop":
            raise ValueError("--retire-dead-adoption is valid only with stop")
        client = HerdrClient(herdr_bin=str(args.herdr_bin))
        target = _target(args)
        queue = os.path.abspath(str(args.queue))
        if args.name is not None or args.command in ("start", "stop", "list", "wait", "goal", "bind-session"):
            return _managed(args, client)
        if args.command == "send":
            result = send(
                client, target, queue, _message(args), ready_timeout=args.ready_timeout,
                working_timeout=args.working_timeout, max_attempts=args.max_attempts,
            )
            json.dump(result.__dict__, sys.stdout, indent=2, sort_keys=True)
            sys.stdout.write("\n")
            return 0
        if args.command == "drain":
            result = drain(
                client, target, queue, ready_timeout=args.ready_timeout,
                working_timeout=args.working_timeout, max_attempts=args.max_attempts,
            )
            json.dump(result.__dict__, sys.stdout, indent=2, sort_keys=True)
            sys.stdout.write("\n")
            if result.blocked is not None:
                return 75
            return 76 if result.quarantined else 0
        if args.command == "status":
            json.dump(status(client, target, queue), sys.stdout, indent=2, sort_keys=True)
            sys.stdout.write("\n")
            return 0
        sys.stdout.write(read(client, target, lines=args.lines))
        return 0
    except (AgentPending, AgentPossiblySubmitted) as exc:
        json.dump(
            {
                "outcome": exc.outcome,
                "message_id": exc.message_id,
                "artifact": exc.artifact,
                "error": str(exc),
                "safe_to_retry": isinstance(exc, AgentPending),
            },
            sys.stdout,
            indent=2,
            sort_keys=True,
        )
        sys.stdout.write("\n")
        if args.command == "stop":
            print(stop_refusal_message(
                exc, prefix="herdr-agent", registry=args.registry, herdr_bin=args.herdr_bin,
                expected_token=args.expected_token,
                expected_record_sha256=args.expected_record_sha256,
            ), file=sys.stderr)
        return exc.exit_code
    except (HerdrRunError, ValueError, TypeError, OSError) as exc:
        if args.command == "stop":
            print(stop_refusal_message(
                exc, prefix="herdr-agent", registry=args.registry, herdr_bin=args.herdr_bin,
                expected_token=args.expected_token,
                expected_record_sha256=args.expected_record_sha256,
            ), file=sys.stderr)
        else:
            print(f"herdr-agent: {exc}", file=sys.stderr)
        return exc.exit_code if isinstance(exc, HerdrRunError) else 2


if __name__ == "__main__":
    raise SystemExit(main())

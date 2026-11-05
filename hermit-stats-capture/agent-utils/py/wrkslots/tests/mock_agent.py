#!/usr/bin/env python3
"""Create a real coordinator-child process tree for wrkslots end-to-end tests."""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path


def _write_json(path: Path, value: object) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def _start_ticks(pid: int) -> int:
    text = Path("/proc", str(pid), "stat").read_text(encoding="ascii")
    close = text.rfind(")")
    fields = text[close + 2 :].split()
    return int(fields[19])


def _command(args: argparse.Namespace) -> int:
    os.chdir(args.cwd)
    held = open(args.hold_file, "rb")  # noqa: SIM115 - deliberately held for process-use scans
    try:
        while True:
            time.sleep(1)
    finally:
        held.close()


def _run_request(payload: dict[str, object], cwd: Path) -> dict[str, object]:
    action = payload.get("action")
    if action == "write":
        relative = Path(str(payload["path"]))
        if relative.is_absolute() or ".." in relative.parts:
            return {"returncode": 2, "stdout": "", "stderr": "unsafe relative path"}
        destination = cwd / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(str(payload.get("content", "")), encoding="utf-8")
        return {"returncode": 0, "stdout": "", "stderr": ""}
    if action != "exec":
        return {"returncode": 2, "stdout": "", "stderr": f"unknown action: {action!r}"}
    raw_argv = payload.get("argv")
    if not isinstance(raw_argv, list):
        return {"returncode": 2, "stdout": "", "stderr": "argv must be a string list"}
    argv: list[str] = []
    for item in raw_argv:
        if not isinstance(item, str):
            return {"returncode": 2, "stdout": "", "stderr": "argv must be a string list"}
        argv.append(item)
    environment = os.environ.copy()
    raw_environment = payload.get("env", {})
    if not isinstance(raw_environment, dict):
        return {"returncode": 2, "stdout": "", "stderr": "env must be an object"}
    environment.update({str(key): str(value) for key, value in raw_environment.items()})
    raw_timeout = payload.get("timeout", 30)
    if isinstance(raw_timeout, bool) or not isinstance(raw_timeout, (int, float)):
        return {"returncode": 2, "stdout": "", "stderr": "timeout must be a number"}
    completed = subprocess.run(
        argv,
        cwd=cwd,
        env=environment,
        text=True,
        capture_output=True,
        check=False,
        timeout=float(raw_timeout),
    )
    return {
        "returncode": completed.returncode,
        "stdout": completed.stdout,
        "stderr": completed.stderr,
    }


def _engine(args: argparse.Namespace) -> int:
    cwd = Path(args.cwd).resolve(strict=True)
    control = Path(args.control_dir).resolve(strict=True)
    os.chdir(cwd)
    command = subprocess.Popen(
        [
            sys.executable,
            str(Path(__file__).resolve()),
            "command",
            "--cwd",
            str(cwd),
            "--hold-file",
            str(cwd / args.hold_file),
        ]
    )
    _write_json(
        Path(args.ready_file),
        {
            "launcher_pid": os.getppid(),
            "engine_pid": os.getpid(),
            "engine_start_ticks": _start_ticks(os.getpid()),
            "command_pid": command.pid,
        },
    )
    try:
        while True:
            requests = sorted(control.glob("request-*.json"))
            if not requests:
                time.sleep(0.02)
                continue
            for request in requests:
                identifier = request.stem.removeprefix("request-")
                try:
                    raw_payload: object = json.loads(request.read_text(encoding="utf-8"))
                    if not isinstance(raw_payload, dict) or not all(
                        isinstance(key, str) for key in raw_payload
                    ):
                        raise ValueError("request must be an object with string keys")
                    payload = {str(key): value for key, value in raw_payload.items()}
                    request.unlink()
                    if payload.get("action") == "stop":
                        _write_json(
                            control / f"response-{identifier}.json",
                            {"returncode": 0, "stdout": "", "stderr": ""},
                        )
                        return 0
                    result = _run_request(payload, cwd)
                except (OSError, ValueError, subprocess.SubprocessError) as exc:
                    result = {"returncode": 125, "stdout": "", "stderr": str(exc)}
                _write_json(control / f"response-{identifier}.json", result)
    finally:
        if command.poll() is None:
            command.terminate()
            try:
                command.wait(timeout=5)
            except subprocess.TimeoutExpired:
                command.kill()
                command.wait(timeout=5)


def _launcher(args: argparse.Namespace) -> int:
    engine = subprocess.Popen(
        [
            sys.executable,
            str(Path(__file__).resolve()),
            "engine",
            "--cwd",
            args.cwd,
            "--control-dir",
            args.control_dir,
            "--ready-file",
            args.ready_file,
            "--hold-file",
            args.hold_file,
        ],
        start_new_session=True,
    )
    stopping = False

    def stop(_signum: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while True:
        returncode = engine.poll()
        if returncode == 0:
            return 0
        if stopping:
            try:
                os.killpg(engine.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                engine.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(engine.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                engine.wait(timeout=5)
            return 0
        if returncode is not None:
            # Keep the launcher alive after an engine crash. Descendant commands remain observable
            # until the coordinator explicitly terminates this process tree.
            time.sleep(0.05)
            continue
        time.sleep(0.05)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="mode", required=True)
    launcher = subparsers.add_parser("launcher")
    engine = subparsers.add_parser("engine")
    command = subparsers.add_parser("command")
    for child in (launcher, engine):
        child.add_argument("--cwd", required=True)
        child.add_argument("--control-dir", required=True)
        child.add_argument("--ready-file", required=True)
        child.add_argument("--hold-file", default="seed.txt")
    command.add_argument("--cwd", required=True)
    command.add_argument("--hold-file", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.mode == "launcher":
        return _launcher(args)
    if args.mode == "engine":
        return _engine(args)
    return _command(args)


if __name__ == "__main__":
    raise SystemExit(main())

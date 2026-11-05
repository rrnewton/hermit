#!/usr/bin/env python3
"""Exercise wrkslots with real Git worktrees, process trees, crashes, and contention."""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
PY_ROOT = PACKAGE_ROOT.parent
MOCK_AGENT = Path(__file__).with_name("mock_agent.py")
MACHINE = "e2ehost"


class HarnessFailure(RuntimeError):
    """One end-to-end command or invariant failed."""


def _integer(value: object, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise HarnessFailure(f"{label} is not an integer: {value!r}")
    return value


def _publish_json(path: Path, value: object) -> None:
    """Publish one control message only after its JSON is complete."""
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


class Trace:
    """Append enough structured evidence to replay a failed seeded run."""

    def __init__(self, path: Path, seed: int) -> None:
        self.path = path
        self.seed = seed

    def add(self, event: str, **fields: object) -> None:
        value = {"time": time.time(), "seed": self.seed, "event": event, **fields}
        with self.path.open("a", encoding="utf-8") as output:
            output.write(json.dumps(value, sort_keys=True) + "\n")


@dataclass(frozen=True)
class TestProject:
    """Paths and environment for one isolated local-only wrkslots project."""

    root: Path
    repository: Path
    remote: Path
    environment: dict[str, str]


def _environment() -> dict[str, str]:
    result = os.environ.copy()
    previous = result.get("PYTHONPATH")
    result["PYTHONPATH"] = str(PY_ROOT) if not previous else f"{PY_ROOT}{os.pathsep}{previous}"
    result["LC_ALL"] = "C"
    return result


def _run(
    argv: Sequence[str],
    *,
    cwd: Path,
    env: dict[str, str],
    expected: int | Sequence[int] = 0,
    timeout: float = 120,
    trace: Trace | None = None,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        list(argv),
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout,
    )
    allowed = {expected} if isinstance(expected, int) else set(expected)
    if trace is not None:
        trace.add(
            "command",
            argv=list(argv),
            cwd=str(cwd),
            returncode=completed.returncode,
            stdout=completed.stdout,
            stderr=completed.stderr,
        )
    if completed.returncode not in allowed:
        raise HarnessFailure(
            f"command returned {completed.returncode}, expected {sorted(allowed)}: "
            f"{' '.join(argv)}\nstdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed


def _git(
    path: Path,
    *args: str,
    env: dict[str, str],
    trace: Trace | None = None,
    expected: int | Sequence[int] = 0,
) -> subprocess.CompletedProcess[str]:
    return _run(
        ["git", "-C", str(path), *args],
        cwd=path,
        env=env,
        expected=expected,
        trace=trace,
    )


_LIVENESS_PROGRAM = '''#!/usr/bin/env python3
import json
import os
import pathlib
import re
import sys

agent = sys.argv[1] if len(sys.argv) == 2 else ""
if not re.fullmatch(r"[A-Za-z0-9._-]+", agent):
    print("invalid agent")
    raise SystemExit(2)
root = pathlib.Path(os.environ["WRKSLOTS_PROJECT_ROOT"])
record = root / "liveness" / f"{agent}.json"
try:
    value = json.loads(record.read_text(encoding="utf-8"))
    pid = int(value["pid"])
    expected = int(value["start_ticks"])
    text = pathlib.Path("/proc", str(pid), "stat").read_text(encoding="ascii")
    close = text.rfind(")")
    actual = int(text[close + 2:].split()[19])
except FileNotFoundError:
    print("recorded process is absent")
    raise SystemExit(0)
except (KeyError, OSError, UnicodeError, ValueError, json.JSONDecodeError) as exc:
    print(f"unverifiable: {exc}")
    raise SystemExit(2)
if actual == expected:
    print(f"pid {pid} generation is alive agent={agent} rc=1")
    raise SystemExit(1)
print(f"pid {pid} generation ended")
raise SystemExit(0)
'''


def _make_project(base: Path, trace: Trace, ttl_seconds: int = 2) -> TestProject:
    root = base / "project"
    repository = root / "product"
    remote = base / "product.git"
    environment = _environment()
    root.mkdir(parents=True)
    (root / "liveness").mkdir()
    liveness = root / "liveness.py"
    liveness.write_text(_LIVENESS_PROGRAM, encoding="utf-8")
    liveness.chmod(0o755)
    _run(
        ["git", "init", "--bare", "--initial-branch=main", str(remote)],
        cwd=base,
        env=environment,
        trace=trace,
    )
    _run(["git", "clone", str(remote), str(repository)], cwd=base, env=environment, trace=trace)
    _git(repository, "config", "user.name", "Wrkslots E2E", env=environment, trace=trace)
    _git(
        repository,
        "config",
        "user.email",
        "wrkslots-e2e@example.invalid",
        env=environment,
        trace=trace,
    )
    (repository / "seed.txt").write_text("seed\n", encoding="utf-8")
    _git(repository, "add", "seed.txt", env=environment, trace=trace)
    _git(repository, "commit", "-m", "seed", env=environment, trace=trace)
    _git(repository, "push", "-u", "origin", "main", env=environment, trace=trace)
    _run(
        [
            sys.executable,
            "-m",
            "wrkslots",
            "--machine",
            MACHINE,
            "init",
            str(root),
            "--heartbeat-ttl-seconds",
            str(ttl_seconds),
            "--liveness-command",
            "liveness.py",
            "--salvage-push-remote",
            str(remote),
        ],
        cwd=root,
        env=environment,
        trace=trace,
    )
    return TestProject(root=root, repository=repository, remote=remote, environment=environment)


def _cli(
    project: TestProject,
    *args: str,
    expected: int | Sequence[int] = 0,
    trace: Trace | None = None,
    wait_lock: float = 10,
) -> subprocess.CompletedProcess[str]:
    return _run(
        [
            sys.executable,
            "-m",
            "wrkslots",
            "--project-root",
            str(project.root),
            "--machine",
            MACHINE,
            "--wait-lock",
            str(wait_lock),
            *args,
        ],
        cwd=project.root,
        env=project.environment,
        expected=expected,
        trace=trace,
    )


class AgentTree:
    """A launcher -> engine -> command tree controlled through durable request files."""

    def __init__(
        self,
        project: TestProject,
        slot: str,
        agent: str,
        trace: Trace,
    ) -> None:
        self.project = project
        self.slot = slot
        self.agent = agent
        self.trace = trace
        self.checkout = project.root / "worktrees" / "slots" / slot / "product"
        self.control = project.root / "controls" / agent
        self.control.mkdir(parents=True)
        self.ready = self.control / "ready.json"
        self.launcher = subprocess.Popen(
            [
                sys.executable,
                str(MOCK_AGENT),
                "launcher",
                "--cwd",
                str(self.checkout),
                "--control-dir",
                str(self.control),
                "--ready-file",
                str(self.ready),
            ],
            cwd=project.root,
            env=project.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.info = self._wait_json(self.ready, timeout=10)
        record = project.root / "liveness" / f"{agent}.json"
        record.write_text(
            json.dumps(
                {
                    "pid": self.info["engine_pid"],
                    "start_ticks": self.info["engine_start_ticks"],
                },
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        trace.add("agent-started", agent=agent, slot=slot, **self.info)
        self.sequence = 0

    @staticmethod
    def _wait_json(path: Path, timeout: float) -> dict[str, object]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                value = json.loads(path.read_text(encoding="utf-8"))
            except (FileNotFoundError, json.JSONDecodeError):
                time.sleep(0.02)
                continue
            if not isinstance(value, dict) or not all(
                isinstance(key, str) for key in value
            ):
                raise HarnessFailure(f"expected JSON object in {path}")
            return {str(key): item for key, item in value.items()}
        raise HarnessFailure(f"timed out waiting for {path}")

    def request(
        self,
        payload: dict[str, object],
        *,
        expected: int | Sequence[int] = 0,
        timeout: float = 30,
    ) -> dict[str, object]:
        self.sequence += 1
        identifier = f"{self.sequence:06d}"
        request = self.control / f"request-{identifier}.json"
        response = self.control / f"response-{identifier}.json"
        _publish_json(request, payload)
        result = self._wait_json(response, timeout=timeout)
        response.unlink()
        allowed = {expected} if isinstance(expected, int) else set(expected)
        returncode = _integer(result.get("returncode"), "agent response returncode")
        self.trace.add(
            "agent-request",
            agent=self.agent,
            slot=self.slot,
            payload=payload,
            result=result,
        )
        if returncode not in allowed:
            raise HarnessFailure(
                f"agent request returned {returncode}, expected {sorted(allowed)}: {payload}\n"
                f"stdout:\n{result.get('stdout', '')}\nstderr:\n{result.get('stderr', '')}"
            )
        return result

    def run(
        self,
        *argv: str,
        expected: int | Sequence[int] = 0,
        timeout: float = 30,
    ) -> dict[str, object]:
        return self.request(
            {"action": "exec", "argv": list(argv)},
            expected=expected,
            timeout=timeout,
        )

    def write(self, relative: str, content: str) -> None:
        self.request({"action": "write", "path": relative, "content": content})

    def wrkslots(self, *args: str, expected: int | Sequence[int] = 0) -> dict[str, object]:
        # Eight workers deliberately serialize append-only mutations through one
        # project lock. The test is about safe convergence under contention, not
        # a ten-second acquisition SLA; the dedicated lock tests retain the
        # bounded-refusal assertion. Keep the request timeout above this wait so
        # the mock agent can return the command's own diagnostic.
        return self.run(
            sys.executable,
            "-m",
            "wrkslots",
            "--project-root",
            str(self.project.root),
            "--machine",
            MACHINE,
            "--wait-lock",
            "30",
            *args,
            expected=expected,
            timeout=45,
        )

    def stop(self) -> None:
        if self.launcher.poll() is not None:
            return
        self.request({"action": "stop"})
        try:
            self.launcher.wait(timeout=10)
        except subprocess.TimeoutExpired as exc:
            raise HarnessFailure(f"launcher {self.launcher.pid} did not stop") from exc
        self.trace.add("agent-stopped", agent=self.agent, slot=self.slot)

    def kill_engine(self) -> None:
        os.kill(_integer(self.info.get("engine_pid"), "engine PID"), signal.SIGKILL)
        self.trace.add(
            "engine-killed",
            agent=self.agent,
            slot=self.slot,
            pid=self.info["engine_pid"],
        )

    def terminate_tree(self) -> None:
        command_pid = _integer(self.info.get("command_pid"), "command PID")
        try:
            os.kill(command_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        if self.launcher.poll() is None:
            self.launcher.terminate()
            try:
                self.launcher.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.launcher.kill()
                self.launcher.wait(timeout=10)
        def command_is_running() -> bool:
            try:
                text = Path("/proc", str(command_pid), "stat").read_text(
                    encoding="ascii"
                )
            except FileNotFoundError:
                return False
            close = text.rfind(")")
            return text[close + 2 : close + 3] not in {"Z", "X"}

        deadline = time.monotonic() + 10
        while command_is_running() and time.monotonic() < deadline:
            time.sleep(0.02)
        if command_is_running():
            try:
                os.kill(command_pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 10
            while command_is_running() and time.monotonic() < deadline:
                time.sleep(0.02)
        if command_is_running():
            raise HarnessFailure(f"command process {command_pid} did not stop")
        self.trace.add("agent-tree-terminated", agent=self.agent, slot=self.slot)


def _create_unbound(project: TestProject, slot: str, agent: str, trace: Trace) -> None:
    _cli(
        project,
        "create",
        slot,
        "--slot-type",
        "agent",
        "--coordinator-authorized",
        "--agent",
        agent,
        "--task",
        f"task-{slot}",
        "--purpose",
        f"e2e scenario {slot}",
        "--coordinator-pid",
        str(os.getpid()),
        "--repo",
        "product=product",
        "--branch",
        f"product=e2e/{slot}",
        trace=trace,
    )


def _adopt(tree: AgentTree) -> None:
    tree.wrkslots(
        "adopt",
        tree.slot,
        "--agent",
        tree.agent,
        "--owner-pid",
        str(tree.info["engine_pid"]),
        "--expected-generation",
        "1",
    )


def _do_and_land_work(project: TestProject, tree: AgentTree, trace: Trace) -> None:
    branch = f"e2e/{tree.slot}"
    tree.run("git", "config", "user.name", "Wrkslots Mock Agent")
    tree.run("git", "config", "user.email", "mock-agent@example.invalid")
    tree.write(f"{tree.slot}.txt", f"work for {tree.slot}\n")
    tree.run("git", "add", f"{tree.slot}.txt")
    tree.run("git", "commit", "-m", f"work {tree.slot}")
    tree.run("git", "push", "-u", "origin", branch)
    _git(project.repository, "fetch", "origin", branch, env=project.environment, trace=trace)
    _git(
        project.repository,
        "merge",
        "--no-ff",
        f"origin/{branch}",
        "-m",
        f"land {tree.slot}",
        env=project.environment,
        trace=trace,
    )
    _git(project.repository, "push", "origin", "main", env=project.environment, trace=trace)
    tree.run("git", "fetch", "origin", "main")


def _finish(tree: AgentTree) -> None:
    tree.wrkslots(
        "finish",
        tree.slot,
        "--agent",
        tree.agent,
        "--owner-pid",
        str(tree.info["engine_pid"]),
        "--expected-generation",
        "1",
        "--validation",
        "wrkslots e2e: pass",
    )


def _remove(project: TestProject, slot: str, trace: Trace, expected: int | Sequence[int] = 0) -> subprocess.CompletedProcess[str]:
    return _cli(
        project,
        "remove",
        slot,
        "--coordinator-pid",
        str(os.getpid()),
        "--expected-generation",
        "1",
        expected=expected,
        trace=trace,
    )


def _state(project: TestProject) -> dict[str, object]:
    path = project.root / "worktrees" / f"ACTIVE.{MACHINE}.json"
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise HarnessFailure(f"active state is not an object: {path}")
    return value


def _record_owner_cgroup_ended(project: TestProject, slot: str) -> None:
    program = """
import dataclasses
import sys
from wrkslots import cli

root, machine, slot = sys.argv[1:]
config = cli._load_config(root, machine)
state = cli._load_active(config)
record = cli._find_record(state, slot)
if record.owner is None:
    raise SystemExit(f"slot {slot} has no recorded owner")
owner = dataclasses.replace(
    record.owner,
    cgroup_path=f"/wrkslots-e2e-ended/{slot}",
)
updated = dataclasses.replace(record, owner=owner)
cli._write_active_state(
    config,
    cli._replace_record(state, updated),
    action="test-owner-cgroup-ended",
    slot=slot,
)
"""
    _run(
        [sys.executable, "-c", program, str(project.root), MACHINE, slot],
        cwd=project.root,
        env=project.environment,
    )


def _assert_present(project: TestProject, slot: str) -> None:
    slots = _state(project).get("slots")
    if not isinstance(slots, list) or slot not in {item.get("slot") for item in slots if isinstance(item, dict)}:
        raise HarnessFailure(f"slot {slot} is absent from active state")
    if not (project.root / "worktrees" / "slots" / slot / "product").is_dir():
        raise HarnessFailure(f"slot {slot} lost its checkout path")


def _assert_removed(project: TestProject, slot: str) -> None:
    slots = _state(project).get("slots")
    if not isinstance(slots, list) or slot in {item.get("slot") for item in slots if isinstance(item, dict)}:
        raise HarnessFailure(f"slot {slot} remains active after removal")
    if (project.root / "worktrees" / "slots" / slot).exists():
        raise HarnessFailure(f"slot {slot} path remains after removal")
    registered = _git(
        project.repository,
        "worktree",
        "list",
        "--porcelain",
        env=project.environment,
    ).stdout
    if f"/worktrees/slots/{slot}/" in registered:
        raise HarnessFailure(f"slot {slot} remains registered with Git")


def _happy_path(base: Path, trace: Trace) -> None:
    project = _make_project(base, trace)
    _create_unbound(project, "happy", "codex-happy", trace)
    tree = AgentTree(project, "happy", "codex-happy", trace)
    try:
        _adopt(tree)
        _do_and_land_work(project, tree, trace)
        _finish(tree)
        tree.stop()
        time.sleep(2.1)
        removal = _remove(project, "happy", trace, expected=(0, 3))
        if removal.returncode == 3:
            if "remains in recorded owner cgroup" not in removal.stderr:
                raise HarnessFailure(
                    f"happy-path removal refused for an unexpected reason: {removal.stderr}"
                )
            _assert_present(project, "happy")
            _record_owner_cgroup_ended(project, "happy")
            _remove(project, "happy", trace)
        _assert_removed(project, "happy")
    finally:
        tree.terminate_tree()


def _forgotten_clean(base: Path, trace: Trace) -> None:
    project = _make_project(base, trace, ttl_seconds=1)
    _create_unbound(project, "forgotten", "codex-forgotten", trace)
    tree = AgentTree(project, "forgotten", "codex-forgotten", trace)
    try:
        _adopt(tree)
        _do_and_land_work(project, tree, trace)
        _finish(tree)
        tree.stop()
        time.sleep(1.1)
        status = _cli(project, "status", "--format", "json", trace=trace)
        value = json.loads(status.stdout)
        rows = value.get("active") if isinstance(value, dict) else None
        if not isinstance(rows, list) or not rows or rows[0].get("heartbeat_expired") is not True:
            raise HarnessFailure("forgotten clean slot was not diagnosed as heartbeat-expired")
        _assert_present(project, "forgotten")
    finally:
        tree.terminate_tree()


def _abandoned_work(base: Path, trace: Trace, *, unpublished_commit: bool) -> None:
    project = _make_project(base, trace, ttl_seconds=1)
    slot = "unpublished" if unpublished_commit else "dirty"
    agent = f"codex-{slot}"
    _create_unbound(project, slot, agent, trace)
    tree = AgentTree(project, slot, agent, trace)
    try:
        _adopt(tree)
        tree.run("git", "config", "user.name", "Wrkslots Mock Agent")
        tree.run("git", "config", "user.email", "mock-agent@example.invalid")
        tree.write("unfinished.txt", "unfinished\n")
        if unpublished_commit:
            tree.run("git", "add", "unfinished.txt")
            tree.run("git", "commit", "-m", "unpublished work")
            tree.wrkslots(
                "finish",
                slot,
                "--agent",
                agent,
                "--owner-pid",
                str(tree.info["engine_pid"]),
                "--expected-generation",
                "1",
                "--validation",
                "expected refusal",
                expected=3,
            )
        tree.kill_engine()
        time.sleep(0.1)
        tree.terminate_tree()
        time.sleep(1.1)
        _record_owner_cgroup_ended(project, slot)
        _remove(project, slot, trace)
        _assert_removed(project, slot)
        refs = _git(
            project.remote,
            "for-each-ref",
            "--format=%(refname)",
            f"refs/salvage/{MACHINE}/{slot}",
            f"refs/heads/salvage/{MACHINE}/{slot}",
            env=project.environment,
            trace=trace,
        ).stdout
        if not refs.strip():
            raise HarnessFailure(f"abandoned {slot} work was removed without remote salvage")
    finally:
        tree.terminate_tree()


def _crashed_descendant(base: Path, trace: Trace) -> None:
    project = _make_project(base, trace)
    _create_unbound(project, "descendant", "codex-descendant", trace)
    tree = AgentTree(project, "descendant", "codex-descendant", trace)
    try:
        _adopt(tree)
        _do_and_land_work(project, tree, trace)
        _finish(tree)
        tree.kill_engine()
        time.sleep(0.1)
        refused = _remove(project, "descendant", trace, expected=3)
        if "process" not in refused.stderr.lower() and "use" not in refused.stderr.lower():
            raise HarnessFailure("live descendant refusal did not report process use")
        _assert_present(project, "descendant")
        tree.terminate_tree()
    finally:
        tree.terminate_tree()


def _worker(project_root: Path, slot: str, agent: str, seconds: float, result: Path) -> int:
    environment = _environment()
    project = TestProject(
        root=project_root,
        repository=project_root / "product",
        remote=project_root.parent / "product.git",
        environment=environment,
    )
    trace = Trace(result.with_suffix(".trace.jsonl"), seed=0)
    go = project_root / "worker-go"
    deadline = time.monotonic() + 30
    while not go.exists():
        if time.monotonic() >= deadline:
            raise HarnessFailure("worker start barrier timed out")
        time.sleep(0.01)
    tree: AgentTree | None = None
    try:
        _create_unbound(project, slot, agent, trace)
        tree = AgentTree(project, slot, agent, trace)
        _adopt(tree)
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            tree.wrkslots(
                "heartbeat",
                slot,
                "--agent",
                agent,
                "--owner-pid",
                str(tree.info["engine_pid"]),
                "--expected-generation",
                "1",
            )
            _cli(project, "status", "--slot", slot, "--format", "json", trace=trace)
            time.sleep(0.03)
        tree.write(f"{slot}-unfinished.txt", "unfinished\n")
        tree.kill_engine()
        time.sleep(0.05)
        tree.terminate_tree()
        result.write_text(json.dumps({"slot": slot, "ok": True}) + "\n", encoding="utf-8")
        return 0
    except Exception as exc:  # noqa: BLE001 - worker must serialize every failure for its parent
        result.write_text(
            json.dumps({"slot": slot, "ok": False, "error": repr(exc)}) + "\n",
            encoding="utf-8",
        )
        return 1
    finally:
        if tree is not None:
            tree.terminate_tree()


def _concurrent_stress(base: Path, trace: Trace, seed: int, workers: int, seconds: float) -> None:
    project = _make_project(base, trace, ttl_seconds=max(1, int(seconds) + 1))
    randomizer = random.Random(seed)
    identifiers = list(range(workers))
    randomizer.shuffle(identifiers)
    processes: list[tuple[subprocess.Popen[str], Path]] = []
    for index in identifiers:
        result = base / f"worker-{index}.json"
        process = subprocess.Popen(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "--worker-project",
                str(project.root),
                "--worker-slot",
                f"stress{index:02d}",
                "--worker-agent",
                f"codex-stress{index:02d}",
                "--worker-result",
                str(result),
                "--seconds",
                str(seconds),
            ],
            cwd=project.root,
            env=project.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        processes.append((process, result))
    (project.root / "worker-go").write_text("go\n", encoding="utf-8")
    failures: list[str] = []
    for process, result in processes:
        try:
            stdout, stderr = process.communicate(timeout=max(60, seconds * 5 + 30))
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=10)
            failures.append(f"worker {process.pid} timed out: {stdout} {stderr}")
            continue
        trace.add(
            "worker-exit",
            pid=process.pid,
            returncode=process.returncode,
            stdout=stdout,
            stderr=stderr,
        )
        try:
            value = json.loads(result.read_text(encoding="utf-8"))
        except (FileNotFoundError, json.JSONDecodeError) as exc:
            failures.append(f"worker {process.pid} has no result: {exc}; {stdout} {stderr}")
            continue
        if process.returncode != 0 or value.get("ok") is not True:
            failures.append(f"worker {process.pid} failed: {value}; {stdout} {stderr}")
    if failures:
        raise HarnessFailure("\n".join(failures))
    slots = _state(project).get("slots")
    if not isinstance(slots, list) or len(slots) != workers:
        raise HarnessFailure(f"stress run recorded {len(slots) if isinstance(slots, list) else 'invalid'} slots, expected {workers}")
    names = [item.get("slot") for item in slots if isinstance(item, dict)]
    if len(names) != len(set(names)):
        raise HarnessFailure(f"stress run recorded duplicate slots: {names}")
    for name in names:
        if not isinstance(name, str):
            raise HarnessFailure("stress run recorded a non-string slot name")
        _assert_present(project, name)
    _cli(project, "status", "--all-machines", "--format", "json", trace=trace)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=1, help="deterministic stress ordering")
    parser.add_argument("--workers", type=int, default=4, help="concurrent coordinators")
    parser.add_argument("--seconds", type=float, default=20, help="compressed active time per stress worker")
    parser.add_argument("--keep", action="store_true", help="retain the temporary project after success")
    parser.add_argument("--worker-project", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--worker-slot", help=argparse.SUPPRESS)
    parser.add_argument("--worker-agent", help=argparse.SUPPRESS)
    parser.add_argument("--worker-result", type=Path, help=argparse.SUPPRESS)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    if args.worker_project is not None:
        if not args.worker_slot or not args.worker_agent or args.worker_result is None:
            raise SystemExit("incomplete worker arguments")
        return _worker(
            args.worker_project,
            args.worker_slot,
            args.worker_agent,
            args.seconds,
            args.worker_result,
        )
    if args.workers < 1 or args.seconds < 0:
        raise SystemExit("--workers must be positive and --seconds must be non-negative")
    root = Path(tempfile.mkdtemp(prefix=f"wrkslots-e2e-{args.seed}-"))
    trace = Trace(root / "trace.jsonl", args.seed)
    trace.add("suite-start", root=str(root), workers=args.workers, seconds=args.seconds)
    scenarios: tuple[tuple[str, Callable[[], None]], ...] = (
        ("happy", lambda: _happy_path(root / "happy", trace)),
        ("forgotten-clean", lambda: _forgotten_clean(root / "forgotten", trace)),
        (
            "dirty-death",
            lambda: _abandoned_work(root / "dirty", trace, unpublished_commit=False),
        ),
        (
            "unpublished-death",
            lambda: _abandoned_work(root / "unpublished", trace, unpublished_commit=True),
        ),
        ("crashed-descendant", lambda: _crashed_descendant(root / "descendant", trace)),
        (
            "concurrent-stress",
            lambda: _concurrent_stress(
                root / "stress", trace, args.seed, args.workers, args.seconds
            ),
        ),
    )
    failures: list[str] = []
    for name, scenario in scenarios:
        trace.add("scenario-start", name=name)
        try:
            scenario()
        except Exception as exc:  # noqa: BLE001 - continue to collect independent findings
            failures.append(f"{name}: {exc}")
            trace.add("scenario-fail", name=name, error=repr(exc))
        else:
            trace.add("scenario-pass", name=name)
    if failures:
        trace.add("suite-fail", failures=failures)
        print(
            f"wrkslots e2e FAILED; seed={args.seed} retained={root}:\n- "
            + "\n- ".join(failures),
            file=sys.stderr,
        )
        return 1
    trace.add("suite-pass")
    if args.keep:
        print(f"wrkslots e2e passed; seed={args.seed} retained={root}")
    else:
        shutil.rmtree(root)
        print(f"wrkslots e2e passed; seed={args.seed}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

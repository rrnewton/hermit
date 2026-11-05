#!/usr/bin/env python3
"""Run one deterministic, family-preserving shard of a pytest selection.

Every shard performs the same collection and partitions the resulting node IDs. Parameterized
cases from one test function stay together, which avoids turning one logical mutation matrix into
concurrent processes. Families are assigned largest-first to the currently lightest shard, using
case count as the weight; ties are stable by family name and shard index.

Examples:
    cd py
    python3 ../scripts/run_pytest_shard.py --shard 0/6 -- \
        -q -c pyproject.toml --rootdir=. wrkslots/tests/test_lifecycle.py \
        -m 'not ordinary_environment'

    python3 scripts/run_pytest_shard.py --self-test
"""

from __future__ import annotations

import argparse
import hashlib
import os
import signal
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import FrameType

import pytest


_CANCELLATION_SIGNALS = (signal.SIGHUP, signal.SIGINT, signal.SIGTERM)
_POLL_SECONDS = 0.01
_TERM_GRACE_SECONDS = 1.0
_SignalHandler = Callable[[int, FrameType | None], object] | int | None


def _family(nodeid: str) -> str:
    """Return the stable test-function identity shared by parameterized cases."""
    return nodeid.split("[", 1)[0]


def _assign(nodeids: Sequence[str], count: int) -> tuple[tuple[str, ...], ...]:
    """Partition node IDs deterministically while keeping whole families together."""
    if count < 1:
        raise ValueError("shard count must be positive")

    families: dict[str, list[str]] = {}
    for nodeid in nodeids:
        families.setdefault(_family(nodeid), []).append(nodeid)

    loads = [0] * count
    shards: list[list[str]] = [[] for _ in range(count)]
    for family, members in sorted(families.items(), key=lambda item: (-len(item[1]), item[0])):
        target = min(range(count), key=lambda index: (loads[index], index))
        shards[target].extend(sorted(members))
        loads[target] += len(members)
    return tuple(tuple(sorted(shard)) for shard in shards)


@dataclass
class _ShardPlugin:
    index: int
    count: int

    def pytest_collection_modifyitems(
        self,
        session: pytest.Session,
        config: pytest.Config,
        items: list[pytest.Item],
    ) -> None:
        """Keep only this shard and report all other items as deselected."""
        del session
        assignments = _assign([item.nodeid for item in items], self.count)
        selected_ids = set(assignments[self.index])
        selected = [item for item in items if item.nodeid in selected_ids]
        deselected = [item for item in items if item.nodeid not in selected_ids]
        # xunit2 does not otherwise retain pytest's canonical node ID. Recording both identities
        # makes later timing files lossless and removes the need to reconstruct paths from class
        # names when rebalancing a future run.
        for item in selected:
            item.user_properties.append(("nodeid", item.nodeid))
            item.user_properties.append(("family", _family(item.nodeid)))
        if deselected:
            config.hook.pytest_deselected(items=deselected)
        items[:] = selected
        digest = hashlib.sha256("\n".join(sorted(selected_ids)).encode()).hexdigest()[:12]
        print(
            f"pytest-shard: {self.index + 1}/{self.count}, "
            f"selected={len(selected)}, digest={digest}",
            flush=True,
        )


def _parse_shard(value: str) -> tuple[int, int]:
    try:
        raw_index, raw_count = value.split("/", 1)
        index, count = int(raw_index), int(raw_count)
    except (ValueError, TypeError) as error:
        raise argparse.ArgumentTypeError("shard must have the form INDEX/COUNT") from error
    if count < 1 or index < 0 or index >= count:
        raise argparse.ArgumentTypeError("shard requires COUNT > 0 and 0 <= INDEX < COUNT")
    return index, count


def _restore_owner_access(path: Path) -> None:
    """Make a test-owned tree removable without following its symlinks."""

    if path.is_symlink() or not path.exists():
        return
    path.chmod(path.stat().st_mode | stat.S_IRWXU)
    if path.is_dir():
        for child in path.iterdir():
            _restore_owner_access(child)


def _remove_temp_root(path: Path) -> None:
    """Remove pytest state even when a permissions test made part of it inaccessible."""

    if not path.exists():
        return
    try:
        shutil.rmtree(path)
    except OSError:
        _restore_owner_access(path)
        shutil.rmtree(path)


def _signal_worker_group(process: subprocess.Popen[bytes], signum: int) -> None:
    """Signal only the private worker session created by this wrapper."""

    if process.pid <= 1 or process.pid == os.getpgrp():
        raise RuntimeError(f"refusing unsafe pytest worker process group {process.pid}")
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass


def _normalize_returncode(returncode: int) -> int:
    """Report signal death with the conventional shell-compatible status."""

    return returncode if returncode >= 0 else 128 - returncode


def _run_supervised(index: int, count: int, pytest_args: Sequence[str]) -> int:
    """Run pytest in an owned session and remove its basetemp on cancellation.

    Python's default SIGTERM disposition bypasses ``finally`` blocks. Keeping the
    wrapper outside pytest gives it a place to record that signal, stop the whole
    pytest process group, reap its leader, and remove the one temp tree it owns.
    """

    pending: list[int] = []
    process: subprocess.Popen[bytes] | None = None
    temp_root: Path | None = None
    previous_handlers: dict[signal.Signals, _SignalHandler] = {}

    def record_cancellation(signum: int, _frame: FrameType | None) -> None:
        if not pending:
            pending.append(signum)
        if process is not None:
            _signal_worker_group(process, signum)

    for signum in _CANCELLATION_SIGNALS:
        previous_handlers[signum] = signal.getsignal(signum)
        signal.signal(signum, record_cancellation)
    try:
        temp_root = Path(
            tempfile.mkdtemp(prefix=f"agent-utils-pytest-{os.getpid()}-{index}-")
        )
        if pending:
            return 128 + pending[0]
        command = [
            sys.executable,
            str(Path(__file__).resolve()),
            "--shard",
            f"{index}/{count}",
            "--_worker-basetemp",
            str(temp_root),
            "--",
            *pytest_args,
        ]
        process = subprocess.Popen(command, start_new_session=True)
        shutdown_started: float | None = None
        hard_killed = False
        while True:
            returncode = process.poll()
            if pending:
                if shutdown_started is None:
                    shutdown_started = time.monotonic()
                    _signal_worker_group(process, pending[0])
                if returncode is not None:
                    # The pytest leader may exit before a child which ignored TERM.
                    _signal_worker_group(process, signal.SIGKILL)
                    return 128 + pending[0]
                if (
                    not hard_killed
                    and time.monotonic() - shutdown_started >= _TERM_GRACE_SECONDS
                ):
                    _signal_worker_group(process, signal.SIGKILL)
                    hard_killed = True
            elif returncode is not None:
                return _normalize_returncode(returncode)
            time.sleep(_POLL_SECONDS)
    finally:
        if process is not None and pending:
            _signal_worker_group(process, signal.SIGKILL)
            try:
                process.wait(timeout=_TERM_GRACE_SECONDS)
            except subprocess.TimeoutExpired:
                _signal_worker_group(process, signal.SIGKILL)
                process.wait()
        if temp_root is not None:
            _remove_temp_root(temp_root)
        for signum, previous in previous_handlers.items():
            signal.signal(signum, previous)


def _process_identity(pid: int) -> tuple[str, int, int] | None:
    """Return Linux process state, process group and start time for a live PID."""

    try:
        raw = Path(f"/proc/{pid}/stat").read_bytes()
    except FileNotFoundError:
        return None
    tail = raw[raw.rfind(b") ") + 2 :].split()
    if len(tail) < 20:
        raise AssertionError(f"short /proc identity record for pid {pid}")
    return tail[0].decode("ascii"), int(tail[2]), int(tail[19])


def _self_test_sigterm_cleanup() -> None:
    """Prove a real wrapper cleans its exact temp tree and worker descendants."""

    fixture_root = Path(tempfile.mkdtemp(prefix="agent-utils-sharder-signal-test-"))
    marker = fixture_root / "child.pid"
    test_file = fixture_root / "test_block.py"
    test_file.write_text(
        """\
import os
from pathlib import Path
import subprocess
import sys
import time


def test_block_until_cancelled():
    child = subprocess.Popen([sys.executable, \"-c\", \"import time; time.sleep(300)\"])
    Path(os.environ[\"SHARD_SIGNAL_TEST_MARKER\"]).write_text(str(child.pid), encoding=\"utf-8\")
    time.sleep(300)
""",
        encoding="utf-8",
    )
    environment = os.environ.copy()
    environment["SHARD_SIGNAL_TEST_MARKER"] = str(marker)
    wrapper: subprocess.Popen[str] | None = None
    child_identity: tuple[str, int, int] | None = None
    child_pid: int | None = None
    temp_root: Path | None = None
    try:
        wrapper = subprocess.Popen(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "--shard",
                "0/1",
                "--",
                "-q",
                str(test_file),
            ],
            cwd=fixture_root,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        deadline = time.monotonic() + 10.0
        prefix = f"agent-utils-pytest-{wrapper.pid}-0-"
        while time.monotonic() < deadline:
            matches = [
                path
                for path in Path(tempfile.gettempdir()).iterdir()
                if path.name.startswith(prefix)
            ]
            if len(matches) == 1:
                temp_root = matches[0]
                break
            if wrapper.poll() is not None:
                break
            time.sleep(_POLL_SECONDS)
        assert temp_root is not None, "wrapper did not create its private pytest basetemp"

        while time.monotonic() < deadline and not marker.exists():
            if wrapper.poll() is not None:
                break
            time.sleep(_POLL_SECONDS)
        assert marker.exists(), "blocking pytest fixture did not start"
        child_pid = int(marker.read_text(encoding="utf-8"))
        child_identity = _process_identity(child_pid)
        assert child_identity is not None

        wrapper.send_signal(signal.SIGTERM)
        output = wrapper.communicate(timeout=5.0)[0]
        assert wrapper.returncode == 128 + signal.SIGTERM, output
        assert not temp_root.exists(), f"SIGTERM left basetemp behind: {temp_root}"

        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            identity = _process_identity(child_pid)
            if identity is None or identity[0] == "Z" or identity[2] != child_identity[2]:
                break
            time.sleep(_POLL_SECONDS)
        else:
            raise AssertionError(f"SIGTERM left pytest child {child_pid} running")
    finally:
        if wrapper is not None and wrapper.poll() is None:
            wrapper.send_signal(signal.SIGTERM)
            try:
                wrapper.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                wrapper.kill()
                wrapper.wait()
        if child_pid is not None and child_identity is not None:
            current = _process_identity(child_pid)
            if current is not None and current[2] == child_identity[2]:
                pgid = current[1]
                if pgid > 1 and pgid != os.getpgrp():
                    try:
                        os.killpg(pgid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        _remove_temp_root(fixture_root)


def _self_test() -> int:
    nodeids = [
        "tests/test_a.py::test_matrix[one]",
        "tests/test_a.py::test_matrix[two]",
        "tests/test_b.py::test_one",
        "tests/test_c.py::test_one",
        "tests/test_d.py::test_one",
    ]
    first = _assign(nodeids, 3)
    second = _assign(list(reversed(nodeids)), 3)
    assert first == second
    flattened = [nodeid for shard in first for nodeid in shard]
    assert sorted(flattened) == sorted(nodeids)
    assert len(flattened) == len(set(flattened))
    assert all(
        not ({nodeids[0], nodeids[1]} & set(shard))
        or {nodeids[0], nodeids[1]} <= set(shard)
        for shard in first
    )
    assert max(map(len, first)) - min(map(len, first)) <= 1
    for bad in ("", "1", "a/2", "-1/2", "2/2", "0/0"):
        try:
            _parse_shard(bad)
        except argparse.ArgumentTypeError:
            pass
        else:
            raise AssertionError(f"accepted invalid shard {bad!r}")
    cleanup_root = Path(tempfile.mkdtemp(prefix="agent-utils-sharder-cleanup-"))
    locked = cleanup_root / "locked"
    locked.mkdir()
    (locked / "state").write_text("fixture\n", encoding="utf-8")
    locked.chmod(0)
    _remove_temp_root(cleanup_root)
    assert not cleanup_root.exists()
    _self_test_sigterm_cleanup()
    print("run_pytest_shard --self-test: PASSED")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shard", type=_parse_shard, metavar="INDEX/COUNT")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--_worker-basetemp", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("pytest_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.self_test:
        if args.shard is not None or args.pytest_args or args._worker_basetemp is not None:
            parser.error("--self-test does not accept --shard or pytest arguments")
        return _self_test()
    if args.shard is None:
        parser.error("--shard is required unless --self-test is used")

    pytest_args = list(args.pytest_args)
    if pytest_args[:1] == ["--"]:
        pytest_args = pytest_args[1:]
    if not pytest_args:
        parser.error("pytest arguments are required after --")

    index, count = args.shard
    if args._worker_basetemp is not None:
        return int(
            pytest.main(
                [*pytest_args, "--basetemp", str(args._worker_basetemp)],
                plugins=[_ShardPlugin(index=index, count=count)],
            )
        )
    return _run_supervised(index, count, pytest_args)


if __name__ == "__main__":
    raise SystemExit(main())

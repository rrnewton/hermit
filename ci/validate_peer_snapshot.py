#!/usr/bin/env python3
"""Persist identity-bound external validate.sh peers observed in /proc.

The production receipt writer invokes this repository file without a custom
proc root.  ``--proc-root`` exists only for isolated causal fixtures that call
this helper directly and cannot emit a Hermit validation receipt.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import fcntl
import json
import os
from pathlib import Path
import signal
import socket
import struct
import sys
import tempfile
import time
from typing import Callable

PF_KTHREAD = 0x00200000
MONITOR_PROTOCOL = "sequence-final-ack-v1"


class SnapshotUnresolved(RuntimeError):
    """The snapshot could not safely classify a candidate process."""


@dataclass(frozen=True)
class ProcessRecord:
    pid: int
    state: str
    ppid: int
    pgid: int
    flags: int
    start_ticks: int
    cgroup: str
    cgroup_path: str
    systemd_unit: str
    systemd_unit_cgroup: str
    argv: tuple[str, ...]

    def public_identity(self) -> dict[str, int | str]:
        return {
            "pid": self.pid,
            "start_ticks": self.start_ticks,
            "pgid": self.pgid,
            "cgroup": self.cgroup,
            "cgroup_path": self.cgroup_path,
            "systemd_unit": self.systemd_unit,
            "systemd_unit_cgroup": self.systemd_unit_cgroup,
        }


def _parse_stat(text: str) -> tuple[str, int, int, int, int]:
    """Return (state, ppid, pgid, flags, start_ticks) from one stat row."""
    close = text.rfind(")")
    if close < 0:
        raise ValueError("missing stat comm terminator")
    fields = text[close + 1 :].split()
    if len(fields) < 20:
        raise ValueError("short stat row")
    state = fields[0]
    if len(state) != 1:
        raise ValueError("malformed stat state")
    return state, int(fields[1]), int(fields[2]), int(fields[6]), int(fields[19])


def _cgroup_identity(text: str) -> tuple[str, str, str, str]:
    lines = [line for line in text.splitlines() if line]
    if not lines:
        raise ValueError("empty cgroup identity")
    unified = next((line for line in lines if line.startswith("0::")), lines[0])
    pieces = unified.split(":", 2)
    if len(pieces) != 3 or not pieces[2].startswith("/"):
        raise ValueError("malformed cgroup identity")
    path = pieces[2]
    components = [component for component in path.split("/") if component]
    service_indexes = [
        index
        for index, component in enumerate(components)
        if component.endswith(".service") and not component.startswith("user@")
    ]
    scope_indexes = [
        index
        for index, component in enumerate(components)
        if component.endswith(".scope")
    ]
    if service_indexes:
        index = service_indexes[-1]
    elif scope_indexes:
        index = scope_indexes[-1]
    else:
        # A well-formed root cgroup is normal for kernel threads.  Keep the
        # observable path and classify the empty unit later from stat/cmdline;
        # do not conflate "no unit" with malformed cgroup evidence.
        return unified, path, "", ""
    unit = components[index]
    unit_cgroup = "/" + "/".join(components[: index + 1])
    return unified, path, unit, unit_cgroup


def read_record(proc_root: Path, pid: int) -> ProcessRecord:
    process = proc_root / str(pid)
    state, ppid, pgid, flags, start_ticks = _parse_stat((process / "stat").read_text())
    argv = tuple(
        value.decode(errors="surrogateescape")
        for value in (process / "cmdline").read_bytes().split(b"\0")
        if value
    )
    if not argv and state not in {"Z", "X", "x"} and not flags & PF_KTHREAD:
        raise ValueError("empty cmdline for live userspace process")
    cgroup, cgroup_path, systemd_unit, systemd_unit_cgroup = _cgroup_identity(
        (process / "cgroup").read_text()
    )
    return ProcessRecord(
        pid,
        state,
        ppid,
        pgid,
        flags,
        start_ticks,
        cgroup,
        cgroup_path,
        systemd_unit,
        systemd_unit_cgroup,
        argv,
    )


def read_start_ticks(proc_root: Path, pid: int) -> int:
    return _parse_stat((proc_root / str(pid) / "stat").read_text())[4]


def _numeric_pids(proc_root: Path) -> list[int]:
    try:
        return sorted(
            int(entry.name) for entry in proc_root.iterdir() if entry.name.isdigit()
        )
    except OSError as error:
        raise SnapshotUnresolved(f"cannot enumerate {proc_root}: {error}") from error


def _vanished_after_enoent(proc_root: Path, pid: int) -> bool:
    """Bracket only a real exit race, never a persistent unreadable record."""
    return pid not in _numeric_pids(proc_root)


def is_direct_validate(argv: tuple[str, ...]) -> bool:
    """Whether argv represents a direct validate.sh script execution."""
    if not argv:
        return False
    command = Path(argv[0]).name
    if command == "validate.sh":
        return True
    if command not in {"bash", "dash", "sh"}:
        return False

    index = 1
    while index < len(argv):
        argument = argv[index]
        if argument in {"-c", "--command"}:
            return False
        if argument == "--":
            index += 1
            break
        if argument in {"-O", "+O", "-o", "+o"}:
            index += 2
            continue
        if argument.startswith(("-", "+")):
            index += 1
            continue
        break
    return index < len(argv) and Path(argv[index]).name == "validate.sh"


def _ancestry(pid: int, records: dict[int, ProcessRecord]) -> tuple[list[int], bool]:
    """Return the identity-bound ancestry and whether it reached a root."""
    chain: list[int] = []
    seen: set[int] = set()
    current = pid
    while current > 0:
        if current in seen:
            return chain, False
        seen.add(current)
        chain.append(current)
        record = records.get(current)
        if record is None:
            return chain, False
        if record.ppid in {0, current}:
            return chain, True
        current = record.ppid
    return chain, True


def collect_peer_snapshot(
    proc_root: Path,
    owner_pid: int,
    *,
    record_reader: Callable[[Path, int], ProcessRecord] = read_record,
    start_reader: Callable[[Path, int], int] = read_start_ticks,
) -> dict:
    """Classify root validate drivers by owner ancestry and systemd unit."""
    records: dict[int, ProcessRecord] = {}
    for pid in _numeric_pids(proc_root):
        try:
            records[pid] = record_reader(proc_root, pid)
        except FileNotFoundError as error:
            if _vanished_after_enoent(proc_root, pid):
                continue
            raise SnapshotUnresolved(
                f"process PID {pid} evidence disappeared but PID remains visible: {error}"
            ) from error
        except (OSError, ValueError) as error:
            raise SnapshotUnresolved(
                f"process PID {pid} evidence is unreadable or malformed: {error}"
            ) from error

    if owner_pid <= 1 or owner_pid not in records:
        raise SnapshotUnresolved(f"validate-lock owner PID {owner_pid} is not live")

    owner = records[owner_pid]
    if not owner.systemd_unit or not owner.systemd_unit_cgroup:
        raise SnapshotUnresolved(
            f"validate-lock owner PID {owner_pid} has no observable systemd unit"
        )
    actual = {pid for pid, record in records.items() if is_direct_validate(record.argv)}
    candidate_roots: list[ProcessRecord] = []
    for pid in sorted(actual):
        chain, _complete = _ancestry(pid, records)
        # Bash subshells retain the script argv. Persist only the root actual
        # validate driver, not every descendant that inherited that cmdline.
        if any(ancestor in actual for ancestor in chain[1:]):
            continue
        candidate_roots.append(records[pid])

    peers: list[dict[str, int | str]] = []
    same_service: list[dict[str, int | str]] = []
    for record in candidate_roots:
        if not record.systemd_unit or not record.systemd_unit_cgroup:
            raise SnapshotUnresolved(
                f"candidate PID {record.pid} has no observable systemd unit"
            )
        try:
            observed_again = start_reader(proc_root, record.pid)
        except FileNotFoundError as error:
            if _vanished_after_enoent(proc_root, record.pid):
                continue
            raise SnapshotUnresolved(
                f"candidate PID {record.pid} identity disappeared but PID remains visible"
            ) from error
        except (OSError, ValueError) as error:
            raise SnapshotUnresolved(
                f"candidate PID {record.pid} identity is unreadable or malformed"
            ) from error
        if observed_again != record.start_ticks:
            raise SnapshotUnresolved(
                f"candidate PID {record.pid} changed start_ticks "
                f"{record.start_ticks}->{observed_again}"
            )
        identity = record.public_identity()
        chain, _complete = _ancestry(record.pid, records)
        if owner_pid in chain:
            identity["classification"] = "owner-ancestry-self"
            same_service.append(identity)
        elif (
            record.systemd_unit == owner.systemd_unit
            and record.systemd_unit_cgroup == owner.systemd_unit_cgroup
        ):
            identity["classification"] = "reparented-same-service-self"
            same_service.append(identity)
        else:
            identity["classification"] = "different-systemd-unit-peer"
            peers.append(identity)
    return {
        "owner": owner.public_identity(),
        "same_service_processes": same_service,
        "peers": peers,
    }


def collect_external_peers(
    proc_root: Path,
    owner_pid: int,
    *,
    record_reader: Callable[[Path, int], ProcessRecord] = read_record,
    start_reader: Callable[[Path, int], int] = read_start_ticks,
) -> list[dict[str, int | str]]:
    return collect_peer_snapshot(
        proc_root,
        owner_pid,
        record_reader=record_reader,
        start_reader=start_reader,
    )["peers"]


def _load_state(path: Path) -> dict:
    if not path.exists():
        return {
            "schema_version": 1,
            "scan_complete": False,
            "indeterminate": False,
            "indeterminate_detail": None,
            "scan_count": 0,
            "first_successful_scan_monotonic_ns": None,
            "last_successful_scan_monotonic_ns": None,
            "max_successful_scan_gap_ns": 0,
            "monitor_protocol": MONITOR_PROTOCOL,
            "monitor_ready": False,
            "monitor_pid": None,
            "monitor_sequence": 0,
            "final_ack_sequence": None,
            "exclusion_kind": "kernel-flock",
            "exclusion_held": False,
            "owner": None,
            "same_service_processes": [],
            "peers": [],
        }
    value = json.loads(path.read_text())
    if (
        not isinstance(value, dict)
        or not isinstance(value.get("peers"), list)
        or not isinstance(value.get("same_service_processes"), list)
    ):
        raise SnapshotUnresolved(f"invalid prior peer state in {path}")
    return value


def persist_state(
    path: Path,
    snapshot: dict,
    *,
    indeterminate: bool = False,
    indeterminate_detail: str | None = None,
    monitor_ready: bool | None = None,
    monitor_pid: int | None = None,
    monitor_sequence: int | None = None,
    final_ack_sequence: int | None = None,
    exclusion_held: bool | None = None,
) -> dict:
    try:
        state = _load_state(path)
    except (SnapshotUnresolved, json.JSONDecodeError):
        if not indeterminate:
            raise
        state = {
            "schema_version": 1,
            "scan_complete": False,
            "indeterminate": True,
            "indeterminate_detail": None,
            "scan_count": 0,
            "first_successful_scan_monotonic_ns": None,
            "last_successful_scan_monotonic_ns": None,
            "max_successful_scan_gap_ns": 0,
            "monitor_protocol": MONITOR_PROTOCOL,
            "monitor_ready": False,
            "monitor_pid": None,
            "monitor_sequence": 0,
            "final_ack_sequence": None,
            "exclusion_kind": "kernel-flock",
            "exclusion_held": False,
            "owner": None,
            "same_service_processes": [],
            "peers": [],
        }
    prior_owner = state.get("owner")
    owner = snapshot.get("owner")
    if prior_owner is not None and owner is not None and prior_owner != owner:
        raise SnapshotUnresolved(
            "validate-lock owner identity changed during monitoring"
        )
    peer_identities = {
        (peer.get("pid"), peer.get("start_ticks")): peer
        for peer in state.get("peers", [])
        if isinstance(peer, dict)
    }
    for peer in snapshot.get("peers", []):
        peer_identities[(peer["pid"], peer["start_ticks"])] = peer
    same_service_identities = {
        (process.get("pid"), process.get("start_ticks")): process
        for process in state.get("same_service_processes", [])
        if isinstance(process, dict)
    }
    for process in snapshot.get("same_service_processes", []):
        same_service_identities[(process["pid"], process["start_ticks"])] = process
    now = time.monotonic_ns()
    successful = not indeterminate and owner is not None
    scan_count = state.get("scan_count", 0)
    first_successful = state.get("first_successful_scan_monotonic_ns")
    last_successful = state.get("last_successful_scan_monotonic_ns")
    max_gap = state.get("max_successful_scan_gap_ns", 0)
    if not isinstance(scan_count, int) or scan_count < 0:
        scan_count = 0
    if not isinstance(max_gap, int) or max_gap < 0:
        max_gap = 0
    if successful:
        if isinstance(last_successful, int):
            gap = now - last_successful
            if gap < 0:
                indeterminate = True
                indeterminate_detail = "monotonic-clock-regressed"
            else:
                max_gap = max(max_gap, gap)
        else:
            first_successful = now
        last_successful = now
        scan_count += 1

    sticky_indeterminate = bool(state.get("indeterminate")) or indeterminate
    sticky_detail = state.get("indeterminate_detail")
    if not isinstance(sticky_detail, str) or not sticky_detail:
        if indeterminate:
            sticky_detail = indeterminate_detail or "snapshot-unresolved"
        else:
            sticky_detail = None
    prior_sequence = state.get("monitor_sequence", 0)
    if not isinstance(prior_sequence, int) or prior_sequence < 0:
        prior_sequence = 0
    if monitor_sequence is None:
        monitor_sequence = prior_sequence + 1
    if monitor_sequence < prior_sequence:
        sticky_indeterminate = True
        sticky_detail = sticky_detail or "monitor-sequence-regressed"
    if monitor_ready is None:
        monitor_ready = bool(state.get("monitor_ready"))
    if monitor_pid is None:
        prior_monitor_pid = state.get("monitor_pid")
        monitor_pid = prior_monitor_pid if isinstance(prior_monitor_pid, int) else None
    if exclusion_held is None:
        exclusion_held = bool(state.get("exclusion_held"))
    if final_ack_sequence is None:
        prior_ack = state.get("final_ack_sequence")
        final_ack_sequence = prior_ack if isinstance(prior_ack, int) else None
    state = {
        "schema_version": 1,
        "scan_complete": successful,
        "indeterminate": sticky_indeterminate,
        "indeterminate_detail": sticky_detail,
        "scan_count": scan_count,
        "first_successful_scan_monotonic_ns": first_successful,
        "last_successful_scan_monotonic_ns": last_successful,
        "max_successful_scan_gap_ns": max_gap,
        "monitor_protocol": MONITOR_PROTOCOL,
        "monitor_ready": monitor_ready,
        "monitor_pid": monitor_pid,
        "monitor_sequence": monitor_sequence,
        "final_ack_sequence": final_ack_sequence,
        "exclusion_kind": "kernel-flock",
        "exclusion_held": exclusion_held,
        "owner": owner if owner is not None else prior_owner,
        "same_service_processes": sorted(
            same_service_identities.values(),
            key=lambda process: (process["pid"], process["start_ticks"]),
        ),
        "peers": sorted(
            peer_identities.values(),
            key=lambda peer: (peer["pid"], peer["start_ticks"]),
        ),
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w", dir=path.parent, prefix=f".{path.name}.", delete=False
    ) as handle:
        json.dump(state, handle, separators=(",", ":"))
        handle.write("\n")
        temporary = Path(handle.name)
    os.replace(temporary, path)
    return state


def _production_exclusion_lock() -> Path:
    """Return the non-caller-selectable per-uid validate exclusion lock."""
    runtime = Path("/run/user") / str(os.getuid())
    if not runtime.is_dir():
        raise SnapshotUnresolved(
            f"canonical runtime directory is unavailable: {runtime}"
        )
    return runtime / "hermit-validate-peer-snapshot.lock"


def _peer_descends_from_controller(
    peer_pid: int, controller_pid: int, controller_start_ticks: int
) -> bool:
    """Bind a socket peer to the exact validation-shell controller identity."""
    current = peer_pid
    seen: set[int] = set()
    while current > 1 and current not in seen:
        seen.add(current)
        try:
            state, parent, _pgid, _flags, start_ticks = _parse_stat(
                (Path("/proc") / str(current) / "stat").read_text()
            )
        except (OSError, ValueError):
            return False
        if state in {"Z", "X", "x"}:
            return False
        if current == controller_pid:
            return start_ticks == controller_start_ticks
        if parent in {0, current}:
            return False
        current = parent
    return False


def _scan_once(
    state_path: Path,
    proc_root: Path,
    owner_pid: int,
    *,
    monitor_ready: bool,
    monitor_sequence: int,
    final_ack_sequence: int | None,
    exclusion_held: bool,
) -> tuple[dict, int]:
    sequence = monitor_sequence + 1
    try:
        snapshot = collect_peer_snapshot(proc_root, owner_pid)
        state = persist_state(
            state_path,
            snapshot,
            monitor_ready=monitor_ready,
            monitor_pid=os.getpid(),
            monitor_sequence=sequence,
            final_ack_sequence=final_ack_sequence,
            exclusion_held=exclusion_held,
        )
    except (SnapshotUnresolved, json.JSONDecodeError) as error:
        state = persist_state(
            state_path,
            {"owner": None, "same_service_processes": [], "peers": []},
            indeterminate=True,
            indeterminate_detail=f"snapshot-unresolved:{error}",
            monitor_ready=monitor_ready,
            monitor_pid=os.getpid(),
            monitor_sequence=sequence,
            final_ack_sequence=final_ack_sequence,
            exclusion_held=exclusion_held,
        )
        print(f"validate-peer-snapshot: unresolved: {error}", file=sys.stderr)
    return state, sequence


def run_monitor(
    state_path: Path,
    control_socket: Path,
    proc_root: Path,
    owner_pid: int,
    controller_pid: int,
    controller_start_ticks: int,
    exclusion_lock: Path,
) -> int:
    """Hold exclusion and serve one sequence-bound final snapshot request."""
    control_socket.parent.mkdir(parents=True, exist_ok=True)
    if control_socket.exists():
        raise SnapshotUnresolved(f"control socket already exists: {control_socket}")

    lock_handle = exclusion_lock.open("a+")
    exclusion_held = True
    try:
        fcntl.flock(lock_handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        exclusion_held = False

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(control_socket))
    os.chmod(control_socket, 0o600)
    server.listen(1)
    server.settimeout(0.25)

    running = True

    def stop(_signum: int, _frame: object) -> None:
        nonlocal running
        running = False

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    sequence = 0
    state, sequence = _scan_once(
        state_path,
        proc_root,
        owner_pid,
        monitor_ready=True,
        monitor_sequence=sequence,
        final_ack_sequence=None,
        exclusion_held=exclusion_held,
    )
    if not exclusion_held:
        state = persist_state(
            state_path,
            {"owner": state.get("owner"), "same_service_processes": [], "peers": []},
            indeterminate=True,
            indeterminate_detail="peer-exclusion-lock-contended",
            monitor_ready=True,
            monitor_pid=os.getpid(),
            monitor_sequence=sequence,
            exclusion_held=False,
        )

    next_scan = time.monotonic() + 1.0
    finalized = False
    try:
        while running:
            try:
                connection, _ = server.accept()
            except TimeoutError:
                connection = None
            if connection is not None:
                with connection:
                    peer_pid, peer_uid, _peer_gid = struct.unpack(
                        "3i",
                        connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12),
                    )
                    if peer_uid != os.getuid() or not _peer_descends_from_controller(
                        peer_pid, controller_pid, controller_start_ticks
                    ):
                        connection.sendall(
                            json.dumps(
                                {
                                    "ok": False,
                                    "error": "unauthorized-controller",
                                    "monitor_pid": os.getpid(),
                                },
                                separators=(",", ":"),
                            ).encode()
                            + b"\n"
                        )
                        print(
                            "validate-peer-snapshot: refused control request from "
                            f"non-controller peer PID {peer_pid}",
                            file=sys.stderr,
                        )
                        continue
                    request = connection.recv(64)
                    if request == b"probe\n" and not finalized:
                        response = {
                            "ok": True,
                            "protocol": MONITOR_PROTOCOL,
                            "monitor_pid": os.getpid(),
                            "sequence": sequence,
                            "exclusion_held": exclusion_held,
                        }
                        connection.sendall(
                            json.dumps(response, separators=(",", ":")).encode() + b"\n"
                        )
                        continue
                    if request != b"final\n" or finalized:
                        connection.sendall(b'{"ok":false,"error":"invalid-request"}\n')
                        continue
                    state, sequence = _scan_once(
                        state_path,
                        proc_root,
                        owner_pid,
                        monitor_ready=True,
                        monitor_sequence=sequence,
                        final_ack_sequence=sequence + 1,
                        exclusion_held=exclusion_held,
                    )
                    finalized = True
                    response = {
                        "ok": True,
                        "protocol": MONITOR_PROTOCOL,
                        "monitor_pid": os.getpid(),
                        "ack_sequence": sequence,
                        "scan_complete": state.get("scan_complete"),
                        "indeterminate": state.get("indeterminate"),
                        "exclusion_held": state.get("exclusion_held"),
                    }
                    connection.sendall(
                        json.dumps(response, separators=(",", ":")).encode() + b"\n"
                    )
            if not finalized and time.monotonic() >= next_scan:
                state, sequence = _scan_once(
                    state_path,
                    proc_root,
                    owner_pid,
                    monitor_ready=True,
                    monitor_sequence=sequence,
                    final_ack_sequence=None,
                    exclusion_held=exclusion_held,
                )
                next_scan = time.monotonic() + 1.0
    finally:
        server.close()
        try:
            control_socket.unlink()
        except FileNotFoundError:
            pass
        lock_handle.close()
    return 0


def request_monitor(control_socket: Path, command: str) -> int:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(600)
    try:
        client.connect(str(control_socket))
        peer_pid, peer_uid, _peer_gid = struct.unpack(
            "3i", client.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        )
        if peer_uid != os.getuid() or peer_pid <= 1:
            raise OSError("monitor socket peer has the wrong kernel identity")
        client.sendall(command.encode() + b"\n")
        response = b""
        while not response.endswith(b"\n"):
            chunk = client.recv(4096)
            if not chunk:
                break
            response += chunk
    except (OSError, TimeoutError) as error:
        print(
            f"validate-peer-snapshot: monitor {command} failed: {error}",
            file=sys.stderr,
        )
        return 2
    finally:
        client.close()
    try:
        value = json.loads(response)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        print(
            f"validate-peer-snapshot: malformed monitor {command} response: {error}",
            file=sys.stderr,
        )
        return 2
    if (
        not isinstance(value, dict)
        or value.get("ok") is not True
        or value.get("protocol") != MONITOR_PROTOCOL
        or value.get("monitor_pid") != peer_pid
        or (command == "final" and not isinstance(value.get("ack_sequence"), int))
        or (command == "probe" and not isinstance(value.get("sequence"), int))
    ):
        print(
            f"validate-peer-snapshot: monitor {command} response refused",
            file=sys.stderr,
        )
        return 2
    print(json.dumps(value, separators=(",", ":")))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--owner-pid", type=int)
    parser.add_argument("--state", type=Path)
    parser.add_argument("--monitor", action="store_true")
    parser.add_argument("--finalize", action="store_true")
    parser.add_argument("--probe", action="store_true")
    parser.add_argument("--control-socket", type=Path)
    parser.add_argument("--controller-pid", type=int)
    parser.add_argument("--controller-start-ticks", type=int)
    parser.add_argument("--fixture-mode", action="store_true")
    parser.add_argument("--fixture-exclusion-lock", type=Path)
    parser.add_argument(
        "--proc-root",
        default=Path("/proc"),
        type=Path,
        help="fixture-only proc tree; validate.sh production never passes this option",
    )
    args = parser.parse_args()
    if args.finalize or args.probe:
        if (
            args.monitor
            or args.control_socket is None
            or (args.finalize and args.probe)
        ):
            parser.error(
                "--finalize/--probe require --control-socket and are exclusive"
            )
        return request_monitor(
            args.control_socket, "final" if args.finalize else "probe"
        )
    if args.owner_pid is None or args.state is None:
        parser.error("snapshot and monitor modes require --owner-pid and --state")
    if args.owner_pid <= 1:
        parser.error("--owner-pid must identify a live non-init process")
    if args.fixture_exclusion_lock is not None and not args.fixture_mode:
        parser.error("--fixture-exclusion-lock requires --fixture-mode")
    if args.monitor:
        if (
            args.control_socket is None
            or args.controller_pid is None
            or args.controller_pid <= 1
            or args.controller_start_ticks is None
            or args.controller_start_ticks <= 0
        ):
            parser.error(
                "--monitor requires --control-socket and a live controller identity"
            )
        exclusion_lock = (
            args.fixture_exclusion_lock
            if args.fixture_mode and args.fixture_exclusion_lock is not None
            else _production_exclusion_lock()
        )
        try:
            return run_monitor(
                args.state,
                args.control_socket,
                args.proc_root,
                args.owner_pid,
                args.controller_pid,
                args.controller_start_ticks,
                exclusion_lock,
            )
        except (OSError, SnapshotUnresolved) as error:
            persist_state(
                args.state,
                {"owner": None, "same_service_processes": [], "peers": []},
                indeterminate=True,
                indeterminate_detail=f"monitor-start-failed:{error}",
                monitor_ready=False,
                exclusion_held=False,
            )
            print(f"validate-peer-snapshot: monitor failed: {error}", file=sys.stderr)
            return 2
    try:
        snapshot = collect_peer_snapshot(args.proc_root, args.owner_pid)
        state = persist_state(args.state, snapshot)
    except (SnapshotUnresolved, json.JSONDecodeError) as error:
        state = persist_state(
            args.state,
            {"owner": None, "same_service_processes": [], "peers": []},
            indeterminate=True,
            indeterminate_detail=f"snapshot-unresolved:{error}",
        )
        print(f"validate-peer-snapshot: unresolved: {error}", file=sys.stderr)
        print(json.dumps(state, separators=(",", ":")))
        return 2
    print(json.dumps(state, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

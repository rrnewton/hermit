#!/usr/bin/env python3
"""Persist identity-bound external validate.sh peers observed in /proc.

The production receipt writer invokes this repository file without a custom
proc root.  ``--proc-root`` exists only for isolated causal fixtures that call
this helper directly and cannot emit a Hermit validation receipt.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import sys
import tempfile
import time
from typing import Callable

MAX_SCAN_GAP_NS = 5_000_000_000

class SnapshotUnresolved(RuntimeError):
    """The snapshot could not safely classify a candidate process."""


@dataclass(frozen=True)
class ProcessRecord:
    pid: int
    ppid: int
    pgid: int
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


def _parse_stat(text: str) -> tuple[int, int, int]:
    """Return (ppid, pgid, start_ticks) from one /proc/PID/stat row."""
    close = text.rfind(")")
    if close < 0:
        raise ValueError("missing stat comm terminator")
    fields = text[close + 1 :].split()
    if len(fields) < 20:
        raise ValueError("short stat row")
    return int(fields[1]), int(fields[2]), int(fields[19])


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
        raise ValueError("cgroup has no observable systemd unit")
    unit = components[index]
    unit_cgroup = "/" + "/".join(components[: index + 1])
    return unified, path, unit, unit_cgroup


def read_record(proc_root: Path, pid: int) -> ProcessRecord:
    process = proc_root / str(pid)
    ppid, pgid, start_ticks = _parse_stat((process / "stat").read_text())
    argv = tuple(
        value.decode(errors="surrogateescape")
        for value in (process / "cmdline").read_bytes().split(b"\0")
        if value
    )
    cgroup, cgroup_path, systemd_unit, systemd_unit_cgroup = _cgroup_identity(
        (process / "cgroup").read_text()
    )
    return ProcessRecord(
        pid,
        ppid,
        pgid,
        start_ticks,
        cgroup,
        cgroup_path,
        systemd_unit,
        systemd_unit_cgroup,
        argv,
    )


def read_start_ticks(proc_root: Path, pid: int) -> int:
    return _parse_stat((proc_root / str(pid) / "stat").read_text())[2]


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
    try:
        entries = list(proc_root.iterdir())
    except OSError as error:
        raise SnapshotUnresolved(f"cannot enumerate {proc_root}: {error}") from error
    for entry in entries:
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            records[pid] = record_reader(proc_root, pid)
        except (OSError, ValueError):
            continue

    if owner_pid <= 1 or owner_pid not in records:
        raise SnapshotUnresolved(f"validate-lock owner PID {owner_pid} is not live")

    owner = records[owner_pid]
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
        try:
            observed_again = start_reader(proc_root, record.pid)
        except (OSError, ValueError) as error:
            raise SnapshotUnresolved(
                f"candidate PID {record.pid} vanished before identity confirmation"
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
            "allowed_max_scan_gap_ns": MAX_SCAN_GAP_NS,
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
            "allowed_max_scan_gap_ns": MAX_SCAN_GAP_NS,
            "owner": None,
            "same_service_processes": [],
            "peers": [],
        }
    prior_owner = state.get("owner")
    owner = snapshot.get("owner")
    if prior_owner is not None and owner is not None and prior_owner != owner:
        raise SnapshotUnresolved("validate-lock owner identity changed during monitoring")
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
    stale = False
    stale_detail: str | None = None
    if successful:
        if isinstance(last_successful, int):
            gap = now - last_successful
            if gap < 0:
                stale = True
            else:
                max_gap = max(max_gap, gap)
                stale = gap > MAX_SCAN_GAP_NS
                if stale:
                    stale_detail = (
                        f"successful-scan-gap-exceeded:{gap}>{MAX_SCAN_GAP_NS}"
                    )
        else:
            first_successful = now
        last_successful = now
        scan_count += 1

    sticky_indeterminate = bool(state.get("indeterminate")) or indeterminate or stale
    sticky_detail = state.get("indeterminate_detail")
    if not isinstance(sticky_detail, str) or not sticky_detail:
        if indeterminate:
            sticky_detail = indeterminate_detail or "snapshot-unresolved"
        elif stale:
            sticky_detail = stale_detail
        else:
            sticky_detail = None
    state = {
        "schema_version": 1,
        "scan_complete": successful,
        "indeterminate": sticky_indeterminate,
        "indeterminate_detail": sticky_detail,
        "scan_count": scan_count,
        "first_successful_scan_monotonic_ns": first_successful,
        "last_successful_scan_monotonic_ns": last_successful,
        "max_successful_scan_gap_ns": max_gap,
        "allowed_max_scan_gap_ns": MAX_SCAN_GAP_NS,
        "owner": owner if owner is not None else prior_owner,
        "same_service_processes": sorted(
            same_service_identities.values(),
            key=lambda process: (process["pid"], process["start_ticks"]),
        ),
        "peers": sorted(
            peer_identities.values(), key=lambda peer: (peer["pid"], peer["start_ticks"])
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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--owner-pid", required=True, type=int)
    parser.add_argument("--state", required=True, type=Path)
    parser.add_argument(
        "--proc-root",
        default=Path("/proc"),
        type=Path,
        help="fixture-only proc tree; validate.sh production never passes this option",
    )
    args = parser.parse_args()
    if args.owner_pid <= 1:
        parser.error("--owner-pid must identify a live non-init process")
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

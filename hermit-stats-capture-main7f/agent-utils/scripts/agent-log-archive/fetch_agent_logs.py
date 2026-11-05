#!/usr/bin/env python3
"""Safely collect agent-harness logs into a private, non-deleting archive.

The script is deliberately standalone: install it beside ``machines.tsv`` and
``log_roots.tsv`` in the archive directory.  It uses only the Python standard
library and external ``rsync``/``ssh`` commands.
"""

from __future__ import annotations

import csv
import datetime as dt
import enum
import fcntl
import hashlib
import io
import json
import os
import re
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Iterable, Sequence
from contextlib import ExitStack
from dataclasses import dataclass, replace
from pathlib import Path, PurePosixPath
from types import FrameType
from typing import TextIO


SSH_OPTIONS = (
    "-n",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=20",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
)
RSYNC_SSH_OPTIONS = SSH_OPTIONS[1:]

#: Wall-clock ceilings on every child process this tool starts.
#:
#: ``ConnectTimeout`` above bounds the TCP connect and NOTHING ELSE. Authentication happens after
#: the connection is up, so an ssh that reaches the host and then blocks talking to a dead agent
#: socket waits forever with ``ConnectTimeout`` satisfied. That is not hypothetical: it is the
#: observed failure -- a fetch sat for eight minutes having transferred nothing, printed no error,
#: and would have sat indefinitely.
#:
#: Generous on purpose. These exist to tell FINITE from INFINITE, not fast from slow: a probe that
#: needs four minutes on a loaded host is fine, one that needs four hours is wedged. Setting them
#: tight would trade a hang for a flake, which is the same outage with a worse explanation.
PROBE_TIMEOUT_S = 240.0
MANIFEST_TIMEOUT_S = 900.0
AGENT_PROBE_TIMEOUT_S = 10.0
#: Exit status for a child this tool killed on its ceiling. 124 is timeout(1)'s convention, so a
#: reader who greps for it finds the convention rather than a number invented here.
TIMED_OUT = 124
REMOTE_MISSING = 44
REMOTE_NOT_DIRECTORY = 45
REMOTE_INACCESSIBLE = 46
#: This tool's own code for "the host never answered", distinct from any remote exit status.
REMOTE_UNREACHABLE = 47
REMOTE_SIZE_FAILED = 47
REMOTE_OUTSIDE_HOME = 48
LOW_FREE_PERCENT = 5.0
SAFE_COMPONENT = re.compile(r"^[A-Za-z0-9._-]+$")
SAFE_HOST = re.compile(r"^[A-Za-z0-9][A-Za-z0-9.-]*$")
SAFE_RELATIVE_SOURCE = re.compile(r"^[A-Za-z0-9._+@/-]+$")
RESERVED_MACHINE_NAMES = frozenset({"_fetch_runs", "_fetch_state"})
SQLITE_SUFFIXES = (".db", ".sqlite", ".sqlite3")
SQLITE_SIDECAR_SUFFIXES = ("-wal", "-shm")
MAX_CONFIG_BYTES = 1024 * 1024
IGNORED_REMOTE_MANIFEST_STDERR_LINES = frozenset(
    {"Meta authorized users only. Usage is subject to monitoring and recording."}
)


USAGE = """Usage: fetch_agent_logs.py [OPTIONS]

Fetch every configured log root from every configured machine. The current
machine is copied locally; other machines are read over SSH. Source files are
never changed, and destination files absent from a source are never deleted.

Options:
  --dry-run                Run rsync with --dry-run; do not change archived logs.
  --check-sources          Check source reachability/presence without rsync.
  --machine SHORT_NAME     Limit to one machine; repeat to select several.
  --root ARCHIVE_NAME      Limit to one root; repeat to select several.
  --min-free-bytes BYTES   Refuse transfers below this free-space reserve.
  --list                   Print the selected machine/root matrix and exit.
  --history                Print the append-only run history and exit.
  --backfill-history       Idempotently migrate/backfill history, print, and exit.
  -h, --help               Show this help.

Examples:
  ./fetch_agent_logs.sh --check-sources
  ./fetch_agent_logs.sh --dry-run --machine build01 --root .codex
  ./fetch_agent_logs.sh --min-free-bytes 10737418240
  ./fetch_agent_logs.sh
"""


class FetchError(Exception):
    """Base class for expected, user-facing failures."""


class ConfigError(FetchError):
    """The inventory is malformed or internally inconsistent."""


class SafetyError(FetchError):
    """A filesystem path or permission invariant is unsafe."""


class LockBusy(FetchError):
    """Another fetch process holds the archive lock."""


class ManifestError(FetchError):
    """A complete source manifest could not be observed."""


class ManifestSourceMissing(ManifestError):
    """The canonical source root vanished while its manifest was scanned."""


class RunInterrupted(FetchError):
    """The process received a termination signal."""


class Mode(enum.Enum):
    FETCH = "fetch"
    DRY_RUN = "dry_run"
    CHECK_SOURCES = "check_sources"


class Requirement(enum.Enum):
    REQUIRED = "required"
    OPTIONAL = "optional"


class ProbeKind(enum.Enum):
    PRESENT = "present"
    MISSING = "missing"
    NOT_DIRECTORY = "not_directory"
    INACCESSIBLE = "inaccessible"
    OUTSIDE_HOME = "outside_home"
    ARCHIVE_OVERLAP = "archive_overlap"
    FAILED = "failed"


class EntryKind(enum.Enum):
    DIRECTORY = "directory"
    FILE = "file"
    SYMLINK = "symlink"
    BLOCK = "block_device"
    CHARACTER = "character_device"
    FIFO = "fifo"
    SOCKET = "socket"
    OTHER = "other"


@dataclass(frozen=True)
class Options:
    mode: Mode
    machines: tuple[str, ...]
    roots: tuple[str, ...]
    list_only: bool
    history_only: bool
    backfill_history: bool
    min_free_bytes: int


@dataclass(frozen=True)
class Machine:
    short_name: str
    host: str
    home: PurePosixPath


@dataclass(frozen=True)
class RootRule:
    machine_scope: str
    archive_name: str
    source_path: PurePosixPath
    requirement: Requirement


@dataclass(frozen=True)
class Configuration:
    machines: tuple[Machine, ...]
    roots: tuple[RootRule, ...]


@dataclass(frozen=True)
class ConfigurationSnapshot:
    configuration: Configuration
    machines_bytes: bytes
    roots_bytes: bytes


@dataclass(frozen=True)
class Operation:
    machine: Machine
    root: RootRule
    source: PurePosixPath
    destination: Path
    local: bool

    @property
    def key(self) -> str:
        return f"{self.machine.short_name}/{self.root.archive_name}"


@dataclass(frozen=True)
class ProbeObservation:
    kind: ProbeKind
    exit_code: int
    estimated_bytes: int | None
    resolved_source: str | None
    detail: str
    started_utc: str
    finished_utc: str
    duration_seconds: float


@dataclass(frozen=True)
class ManifestEntry:
    path: str
    kind: EntryKind
    size: int
    mtime_ns: int
    link_target: str | None


@dataclass(frozen=True)
class ManifestScan:
    entries: tuple[ManifestEntry, ...]
    notices: tuple[str, ...]

    @property
    def regular_bytes(self) -> int:
        return sum(entry.size for entry in self.entries if entry.kind is EntryKind.FILE)


@dataclass(frozen=True)
class RsyncMetrics:
    total_files: int | None = None
    transferred_files: int | None = None
    total_file_size: int | None = None
    transferred_file_size: int | None = None
    literal_data: int | None = None
    matched_data: int | None = None
    bytes_sent: int | None = None
    bytes_received: int | None = None


@dataclass(frozen=True)
class RsyncOutcome:
    exit_code: int
    metrics: RsyncMetrics


@dataclass(frozen=True)
class WarningRecord:
    timestamp_utc: str
    machine: str
    archive_name: str
    code: str
    path: str
    detail: str
    source_size: int | None = None
    destination_size: int | None = None


@dataclass(frozen=True)
class Comparison:
    warnings: tuple[WarningRecord, ...]
    errors: tuple[str, ...]
    retained_entries: int
    retained_bytes: int
    stale_sqlite_sidecars: int
    retained_sqlite_databases: int
    rewrite_warnings: int


@dataclass(frozen=True)
class OperationResult:
    machine: str
    archive_name: str
    requirement: str
    status: str
    exit_code: int
    started_utc: str
    finished_utc: str
    duration_seconds: float
    source_estimated_bytes: int | None
    manifest_entries: int
    manifest_regular_bytes: int
    destination_before_bytes: int
    destination_after_bytes: int
    rsync_total_files: int | None
    rsync_transferred_files: int | None
    rsync_total_file_size: int | None
    rsync_transferred_file_size: int | None
    rsync_literal_data: int | None
    rsync_matched_data: int | None
    rsync_bytes_sent: int | None
    rsync_bytes_received: int | None
    retained_destination_entries: int
    retained_destination_bytes: int
    stale_sqlite_sidecars: int
    retained_sqlite_databases: int
    rewrite_warnings: int
    warning_count: int
    detail: str

    @property
    def successful(self) -> bool:
        return self.status in {
            "fetched",
            "dry_run_complete",
            "present",
            "missing_optional",
        }


@dataclass(frozen=True)
class DiskSnapshot:
    total: int
    used: int
    free: int


@dataclass(frozen=True)
class RunSummary:
    run_id: str
    started_utc: str
    finished_utc: str
    duration_seconds: float
    mode: str
    status: str
    exit_code: int
    expected_operations: int
    recorded_operations: int
    successful_operations: int
    failed_operations: int
    optional_missing: int
    probed_operations: int
    rsync_attempted_operations: int
    rsync_metrics_complete_operations: int
    rsync_metrics_unknown_operations: int
    manifest_entries: int
    manifest_regular_bytes: int
    rsync_transferred_files: int
    rsync_transferred_file_size: int
    rsync_literal_data: int
    rsync_matched_data: int
    rsync_bytes_sent: int
    rsync_bytes_received: int
    retained_destination_entries: int
    retained_destination_bytes: int
    stale_sqlite_sidecars: int
    retained_sqlite_databases: int
    rewrite_warnings: int
    warning_count: int
    disk_total_bytes: int
    disk_free_before_bytes: int
    disk_free_after_bytes: int


def utc_now() -> str:
    """Return a stable millisecond UTC timestamp."""
    value = dt.datetime.now(tz=dt.timezone.utc).isoformat(timespec="milliseconds")
    return value.replace("+00:00", "Z")


def make_run_id() -> str:
    timestamp = dt.datetime.now(tz=dt.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    return f"{timestamp}-{os.getpid()}"


def _raise_run_interrupted(signum: int, frame: FrameType | None) -> None:
    del frame
    try:
        signal_name = signal.Signals(signum).name
    except ValueError:
        signal_name = str(signum)
    raise RunInterrupted(f"run interrupted by {signal_name}")


def parse_options(argv: Sequence[str]) -> Options | None:
    mode = Mode.FETCH
    machines: list[str] = []
    roots: list[str] = []
    list_only = False
    history_only = False
    backfill_history = False
    min_free_bytes = 0
    mode_was_set = False
    index = 0
    while index < len(argv):
        argument = argv[index]
        if argument in {"-h", "--help"}:
            print(USAGE, end="")
            return None
        if argument in {"--dry-run", "--check-sources"}:
            if mode_was_set:
                raise ConfigError("choose only one execution mode")
            mode = Mode.DRY_RUN if argument == "--dry-run" else Mode.CHECK_SOURCES
            mode_was_set = True
            index += 1
            continue
        if argument == "--list":
            list_only = True
            index += 1
            continue
        if argument == "--history":
            history_only = True
            index += 1
            continue
        if argument == "--backfill-history":
            backfill_history = True
            history_only = True
            index += 1
            continue
        if argument in {"--machine", "--root", "--min-free-bytes"}:
            if index + 1 >= len(argv):
                raise ConfigError(f"{argument} needs a value")
            value = argv[index + 1]
            if argument == "--machine":
                machines.append(value)
            elif argument == "--root":
                roots.append(value)
            else:
                try:
                    min_free_bytes = int(value)
                except ValueError as error:
                    raise ConfigError("--min-free-bytes must be an integer") from error
                if min_free_bytes < 0:
                    raise ConfigError("--min-free-bytes may not be negative")
            index += 2
            continue
        raise ConfigError(f"unknown option: {argument}")
    if history_only and (
        mode_was_set
        or list_only
        or machines
        or roots
        or min_free_bytes
    ):
        raise ConfigError(
            "--history/--backfill-history may not be combined with fetch options"
        )
    return Options(
        mode=mode,
        machines=tuple(machines),
        roots=tuple(roots),
        list_only=list_only,
        history_only=history_only,
        backfill_history=backfill_history,
        min_free_bytes=min_free_bytes,
    )


def _validate_component(label: str, value: str) -> None:
    if SAFE_COMPONENT.fullmatch(value) is None or value in {".", ".."}:
        raise ConfigError(f"{label} must be one safe path component: {value}")


def _validate_absolute_home(value: str, path: Path, line: int) -> PurePosixPath:
    if not value.startswith("/") or any(ord(character) < 32 for character in value):
        raise ConfigError(f"{path}:{line}: machine home must be an absolute path")
    if ":" in value:
        raise ConfigError(f"{path}:{line}: machine home may not contain ':'")
    home = PurePosixPath(value)
    if (
        not home.is_absolute()
        or any(part in {".", ".."} for part in home.parts)
        or str(home) != value
    ):
        raise ConfigError(f"{path}:{line}: machine home must be normalized and absolute")
    return home


def _validate_relative_source(value: str, path: Path, line: int) -> PurePosixPath:
    if SAFE_RELATIVE_SOURCE.fullmatch(value) is None:
        raise ConfigError(f"{path}:{line}: unsafe source path: {value}")
    source = PurePosixPath(value)
    if (
        source.is_absolute()
        or not source.parts
        or any(part in {"", ".", ".."} for part in source.parts)
        or str(source) != value
    ):
        raise ConfigError(
            f"{path}:{line}: source path must remain below the configured home"
        )
    return source


def _read_config_file(path: Path) -> tuple[bytes, str]:
    flags = os.O_RDONLY
    if hasattr(os, "O_CLOEXEC"):
        flags |= os.O_CLOEXEC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ConfigError(f"cannot safely open {path}: {error}") from error
    try:
        with os.fdopen(descriptor, "rb") as handle:
            metadata = os.fstat(handle.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
                raise ConfigError(f"config must be an owned regular file: {path}")
            contents = handle.read(MAX_CONFIG_BYTES + 1)
    except OSError as error:
        raise ConfigError(f"cannot read {path}: {error}") from error
    if len(contents) > MAX_CONFIG_BYTES:
        raise ConfigError(f"config exceeds {MAX_CONFIG_BYTES} bytes: {path}")
    try:
        text = contents.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ConfigError(f"config is not valid UTF-8: {path}: {error}") from error
    return contents, text


def _read_rows(path: Path, text: str, columns: int) -> list[tuple[int, list[str]]]:
    rows: list[tuple[int, list[str]]] = []
    with io.StringIO(text, newline="") as handle:
        reader = csv.reader(handle, delimiter="\t")
        try:
            for line, row in enumerate(reader, start=1):
                if not row or (len(row) == 1 and row[0] == ""):
                    continue
                if row[0].startswith("#"):
                    continue
                if len(row) != columns:
                    raise ConfigError(
                        f"{path}:{line}: expected {columns} tab-separated columns, "
                        f"found {len(row)}"
                    )
                if any(field == "" for field in row):
                    raise ConfigError(f"{path}:{line}: fields may not be empty")
                rows.append((line, row))
        except csv.Error as error:
            raise ConfigError(f"cannot parse {path}: {error}") from error
    return rows


def load_configuration(
    machines_file: Path, roots_file: Path
) -> ConfigurationSnapshot:
    machines_bytes, machines_text = _read_config_file(machines_file)
    roots_bytes, roots_text = _read_config_file(roots_file)
    machines: list[Machine] = []
    seen_names: set[str] = set()
    seen_endpoints: set[tuple[str, PurePosixPath]] = set()
    for line, row in _read_rows(machines_file, machines_text, 3):
        short_name, host, raw_home = row
        _validate_component("machine short name", short_name)
        if short_name in RESERVED_MACHINE_NAMES:
            raise ConfigError(
                f"{machines_file}:{line}: reserved machine short name: {short_name}"
            )
        if SAFE_HOST.fullmatch(host) is None:
            raise ConfigError(f"{machines_file}:{line}: unsafe host: {host}")
        home = _validate_absolute_home(raw_home, machines_file, line)
        if short_name in seen_names:
            raise ConfigError(
                f"{machines_file}:{line}: duplicate machine short name: {short_name}"
            )
        endpoint = (host, home)
        if endpoint in seen_endpoints:
            raise ConfigError(
                f"{machines_file}:{line}: duplicate machine endpoint: {host}:{home}"
            )
        seen_names.add(short_name)
        seen_endpoints.add(endpoint)
        machines.append(Machine(short_name=short_name, host=host, home=home))
    if not machines:
        raise ConfigError(f"{machines_file}: no machine rows")

    roots: list[RootRule] = []
    seen_rules: set[tuple[str, str]] = set()
    for line, row in _read_rows(roots_file, roots_text, 4):
        machine_scope, archive_name, raw_source, raw_requirement = row
        if machine_scope != "*":
            _validate_component("root machine scope", machine_scope)
            if machine_scope not in seen_names:
                raise ConfigError(
                    f"{roots_file}:{line}: unknown machine scope: {machine_scope}"
                )
        _validate_component("archive name", archive_name)
        source_path = _validate_relative_source(raw_source, roots_file, line)
        try:
            requirement = Requirement(raw_requirement)
        except ValueError as error:
            raise ConfigError(
                f"{roots_file}:{line}: requirement must be required or optional"
            ) from error
        rule_key = (machine_scope, archive_name)
        if rule_key in seen_rules:
            raise ConfigError(
                f"{roots_file}:{line}: duplicate root scope/archive pair: "
                f"{machine_scope}/{archive_name}"
            )
        seen_rules.add(rule_key)
        roots.append(
            RootRule(
                machine_scope=machine_scope,
                archive_name=archive_name,
                source_path=source_path,
                requirement=requirement,
            )
        )
    if not roots:
        raise ConfigError(f"{roots_file}: no log-root rows")

    destinations: set[tuple[str, str]] = set()
    for machine in machines:
        for root in roots:
            if root.machine_scope not in {"*", machine.short_name}:
                continue
            key = (machine.short_name, root.archive_name)
            if key in destinations:
                raise ConfigError(
                    "multiple root rows resolve to destination "
                    f"{machine.short_name}/{root.archive_name}"
                )
            destinations.add(key)
    return ConfigurationSnapshot(
        configuration=Configuration(machines=tuple(machines), roots=tuple(roots)),
        machines_bytes=machines_bytes,
        roots_bytes=roots_bytes,
    )


def _is_local_machine(machine: Machine, local_short: str, local_fqdn: str) -> bool:
    local_hosts = {
        local_short,
        local_fqdn,
        socket.gethostname(),
        "localhost",
        "127.0.0.1",
    }
    host_is_local = machine.host in local_hosts
    if machine.short_name == local_short and not host_is_local:
        raise ConfigError(
            f"machine {machine.short_name} claims the local short name but host "
            f"{machine.host} is not a local endpoint"
        )
    return host_is_local


def build_operations(
    configuration: Configuration, options: Options, archive_dir: Path
) -> tuple[Operation, ...]:
    machine_names = {machine.short_name for machine in configuration.machines}
    root_names = {root.archive_name for root in configuration.roots}
    for selected in options.machines:
        if selected not in machine_names:
            raise ConfigError(f"unknown machine: {selected}")
    for selected in options.roots:
        if selected not in root_names:
            raise ConfigError(f"unknown root: {selected}")
    selected_machines = set(options.machines)
    selected_roots = set(options.roots)
    local_short = socket.gethostname().split(".", maxsplit=1)[0]
    local_fqdn = socket.getfqdn()
    operations: list[Operation] = []
    for machine in configuration.machines:
        if selected_machines and machine.short_name not in selected_machines:
            continue
        for root in configuration.roots:
            if root.machine_scope not in {"*", machine.short_name}:
                continue
            if selected_roots and root.archive_name not in selected_roots:
                continue
            source = machine.home / root.source_path
            destination = archive_dir / machine.short_name / root.archive_name
            operations.append(
                Operation(
                    machine=machine,
                    root=root,
                    source=source,
                    destination=destination,
                    local=_is_local_machine(machine, local_short, local_fqdn),
                )
            )
    if not operations:
        raise ConfigError("the selection contains no operations")
    return tuple(operations)


def print_configuration(
    configuration: Configuration, operations: Sequence[Operation]
) -> None:
    print("MACHINES")
    print("short_name\thost\thome")
    for machine in configuration.machines:
        print(f"{machine.short_name}\t{machine.host}\t{machine.home}")
    print("\nLOG ROOTS")
    print("machine_scope\tarchive_name\tsource_path\trequirement")
    for root in configuration.roots:
        print(
            f"{root.machine_scope}\t{root.archive_name}\t{root.source_path}\t"
            f"{root.requirement.value}"
        )
    print("\nFETCH MATRIX")
    print("machine\tarchive_name\tsource\tdestination\trequirement\ttransport")
    for operation in operations:
        transport = "local" if operation.local else "ssh"
        print(
            f"{operation.machine.short_name}\t{operation.root.archive_name}\t"
            f"{operation.machine.host}:{operation.source}\t{operation.destination}\t"
            f"{operation.root.requirement.value}\t{transport}"
        )


RESULT_COLUMNS = (
    "machine",
    "archive_name",
    "requirement",
    "status",
    "exit_code",
    "started_utc",
    "finished_utc",
    "duration_seconds",
    "source_estimated_bytes",
    "manifest_entries",
    "manifest_regular_bytes",
    "destination_before_bytes",
    "destination_after_bytes",
    "rsync_total_files",
    "rsync_transferred_files",
    "rsync_total_file_size",
    "rsync_transferred_file_size",
    "rsync_literal_data",
    "rsync_matched_data",
    "rsync_bytes_sent",
    "rsync_bytes_received",
    "retained_destination_entries",
    "retained_destination_bytes",
    "stale_sqlite_sidecars",
    "retained_sqlite_databases",
    "rewrite_warnings",
    "warning_count",
    "detail",
)

HISTORY_COLUMNS = (
    "run_id",
    "started_utc",
    "finished_utc",
    "duration_seconds",
    "mode",
    "status",
    "exit_code",
    "expected_operations",
    "recorded_operations",
    "successful_operations",
    "failed_operations",
    "optional_missing",
    "probed_operations",
    "rsync_attempted_operations",
    "rsync_metrics_complete_operations",
    "rsync_metrics_unknown_operations",
    "manifest_entries",
    "manifest_regular_bytes",
    "rsync_transferred_files",
    "rsync_transferred_file_size",
    "rsync_literal_data",
    "rsync_matched_data",
    "rsync_bytes_sent",
    "rsync_bytes_received",
    "legacy_transferred_file_bytes_approx",
    "metrics_provenance",
    "retained_destination_entries",
    "retained_destination_bytes",
    "stale_sqlite_sidecars",
    "retained_sqlite_databases",
    "rewrite_warnings",
    "warning_count",
    "disk_total_bytes",
    "disk_free_before_bytes",
    "disk_free_after_bytes",
    "receipt",
)
LEGACY_HISTORY_COLUMNS = (
    "run_id",
    "started_utc",
    "finished_utc",
    "duration_seconds",
    "mode",
    "status",
    "exit_code",
    "expected_operations",
    "attempted_operations",
    "transferred_file_bytes_approx",
)


def validate_history_file(history_path: Path) -> None:
    if not os.path.lexists(history_path):
        return
    metadata = history_path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise SafetyError(f"history path is not a regular file: {history_path}")
    if metadata.st_uid != os.getuid():
        raise SafetyError(f"history file is not owned by the current user: {history_path}")
    if metadata.st_size:
        header, rows = _read_tsv_table(history_path)
        if header != HISTORY_COLUMNS:
            raise SafetyError(
                f"history header does not match this fetcher version: {history_path}"
            )
        seen: set[str] = set()
        for row in rows:
            run_id = row["run_id"]
            if not run_id or run_id in seen:
                raise SafetyError(
                    f"duplicate or empty run_id in history: {run_id!r}"
                )
            seen.add(run_id)
    flags = os.O_WRONLY | os.O_APPEND
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(history_path, flags)
    except OSError as error:
        raise SafetyError(f"history file is not safely appendable: {history_path}: {error}") from error
    os.close(descriptor)


def _field(value: object) -> str:
    if value is None:
        return ""
    return str(value).replace("\t", "\\t").replace("\r", "\\r").replace("\n", "\\n")


def _write_tsv(handle: TextIO, values: Iterable[object]) -> None:
    handle.write("\t".join(_field(value) for value in values) + "\n")
    handle.flush()


def _write_schema_row(
    handle: TextIO, columns: Sequence[str], values: Iterable[object]
) -> None:
    materialized = tuple(values)
    if len(materialized) != len(columns):
        raise FetchError(
            f"internal TSV schema mismatch: {len(materialized)} values for "
            f"{len(columns)} columns"
        )
    _write_tsv(handle, materialized)


def _open_exclusive_text(path: Path, mode: int = 0o600) -> TextIO:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(path, flags, mode)
    return os.fdopen(descriptor, "w", encoding="utf-8", newline="")


def _read_tsv_table(path: Path) -> tuple[tuple[str, ...], list[dict[str, str]]]:
    try:
        with path.open("r", encoding="utf-8", newline="") as handle:
            raw_rows = list(csv.reader(handle, delimiter="\t"))
    except (OSError, csv.Error) as error:
        raise SafetyError(f"cannot parse TSV file {path}: {error}") from error
    if not raw_rows:
        return (), []
    header = tuple(raw_rows[0])
    rows: list[dict[str, str]] = []
    for line, raw_row in enumerate(raw_rows[1:], start=2):
        if len(raw_row) != len(header):
            raise SafetyError(
                f"{path}:{line}: expected {len(header)} columns, found {len(raw_row)}"
            )
        rows.append(dict(zip(header, raw_row, strict=True)))
    return header, rows


def _blank_history_row() -> dict[str, str]:
    return {column: "" for column in HISTORY_COLUMNS}


def _legacy_history_row(row: dict[str, str]) -> dict[str, str]:
    converted = _blank_history_row()
    for field in (
        "run_id",
        "started_utc",
        "finished_utc",
        "duration_seconds",
        "mode",
        "status",
        "exit_code",
        "expected_operations",
    ):
        converted[field] = row[field]
    attempted = row["attempted_operations"]
    converted["recorded_operations"] = attempted
    converted["probed_operations"] = attempted
    converted["legacy_transferred_file_bytes_approx"] = row[
        "transferred_file_bytes_approx"
    ]
    converted["metrics_provenance"] = (
        "legacy_approximate"
        if converted["legacy_transferred_file_bytes_approx"]
        else "unknown"
    )
    converted["receipt"] = f"_fetch_runs/{row['run_id']}"
    return converted


def _read_receipt_fields(run_dir: Path) -> dict[str, str] | None:
    run_file = run_dir / "run.tsv"
    if not run_file.is_file() or run_file.is_symlink():
        return None
    try:
        with run_file.open("r", encoding="utf-8", newline="") as handle:
            rows = list(csv.reader(handle, delimiter="\t"))
    except (OSError, csv.Error) as error:
        raise SafetyError(f"cannot parse receipt metadata {run_file}: {error}") from error
    fields: dict[str, str] = {}
    for line, row in enumerate(rows, start=1):
        if row == ["field", "value"]:
            continue
        if len(row) != 2:
            raise SafetyError(f"{run_file}:{line}: expected two columns")
        key, value = row
        if key in fields:
            raise SafetyError(f"{run_file}:{line}: duplicate field: {key}")
        fields[key] = value
    required = {"run_id", "started_utc", "finished_utc", "mode", "status", "exit_code"}
    if not required.issubset(fields):
        return None
    if fields["run_id"] != run_dir.name:
        raise SafetyError(
            f"receipt run_id {fields['run_id']} does not match directory {run_dir.name}"
        )
    return fields


def _receipt_result_statuses(run_dir: Path) -> list[str]:
    results_file = run_dir / "results.tsv"
    if not results_file.is_file() or results_file.is_symlink():
        return []
    header, rows = _read_tsv_table(results_file)
    if "status" not in header:
        raise SafetyError(f"receipt results lack a status column: {results_file}")
    return [row["status"] for row in rows]


def _seal_recovered_receipt(run_dir: Path) -> None:
    paths = (run_dir, *run_dir.rglob("*"))
    already_sealed = True
    for path in paths:
        metadata = path.lstat()
        if metadata.st_uid != os.getuid():
            raise SafetyError(f"receipt path is not owned by the current user: {path}")
        if stat.S_IMODE(metadata.st_mode) & 0o077:
            raise SafetyError(f"receipt path is not private: {path}")
        if stat.S_ISLNK(metadata.st_mode):
            raise SafetyError(f"receipt unexpectedly contains a symlink: {path}")
        if stat.S_ISDIR(metadata.st_mode):
            already_sealed = already_sealed and stat.S_IMODE(metadata.st_mode) == 0o500
        elif stat.S_ISREG(metadata.st_mode):
            already_sealed = already_sealed and stat.S_IMODE(metadata.st_mode) == 0o400
        else:
            raise SafetyError(f"receipt contains an unsupported file type: {path}")
    if already_sealed:
        return
    for path in run_dir.rglob("*"):
        if path.is_file():
            descriptor = os.open(path, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    _seal_receipt_tree(run_dir)
    parent_descriptor = os.open(run_dir.parent, os.O_RDONLY)
    try:
        os.fsync(parent_descriptor)
    finally:
        os.close(parent_descriptor)


def _duration_between(started: str, finished: str) -> str:
    try:
        start_value = dt.datetime.fromisoformat(started.replace("Z", "+00:00"))
        finish_value = dt.datetime.fromisoformat(finished.replace("Z", "+00:00"))
    except ValueError:
        return ""
    return f"{max((finish_value - start_value).total_seconds(), 0.0):.6f}"


def _metrics_provenance(attempted: int, unknown: int) -> str:
    if attempted == 0:
        return "not_applicable"
    if unknown == 0:
        return "rsync_stats_exact"
    return "partial_rsync_stats"


def _history_row_from_receipt(
    archive_dir: Path, run_dir: Path
) -> dict[str, str] | None:
    fields = _read_receipt_fields(run_dir)
    if fields is None:
        return None
    if fields["status"] not in {"complete", "complete_with_warnings", "failed"}:
        return None
    statuses = _receipt_result_statuses(run_dir)
    try:
        expected = int(fields.get("expected_operations", ""))
        int(fields["exit_code"])
    except ValueError:
        return None
    if expected != len(statuses):
        return None
    successful_statuses = {
        "fetched",
        "dry_run_complete",
        "present",
        "missing_optional",
    }
    successful = sum(status in successful_statuses for status in statuses)
    row = _blank_history_row()
    for name in (
        "run_id",
        "started_utc",
        "finished_utc",
        "duration_seconds",
        "mode",
        "status",
        "exit_code",
        "expected_operations",
        "recorded_operations",
        "successful_operations",
        "failed_operations",
        "optional_missing",
        "probed_operations",
        "rsync_attempted_operations",
        "rsync_metrics_complete_operations",
        "rsync_metrics_unknown_operations",
        "manifest_entries",
        "manifest_regular_bytes",
        "rsync_transferred_files",
        "rsync_transferred_file_size",
        "rsync_literal_data",
        "rsync_matched_data",
        "rsync_bytes_sent",
        "rsync_bytes_received",
        "retained_destination_entries",
        "retained_destination_bytes",
        "stale_sqlite_sidecars",
        "retained_sqlite_databases",
        "rewrite_warnings",
        "warning_count",
        "disk_total_bytes",
        "disk_free_before_bytes",
        "disk_free_after_bytes",
    ):
        row[name] = fields.get(name, "")
    if not row["duration_seconds"]:
        row["duration_seconds"] = _duration_between(
            row["started_utc"], row["finished_utc"]
        )
    recorded = len(statuses)
    if not row["recorded_operations"]:
        row["recorded_operations"] = str(recorded)
    if not row["successful_operations"]:
        row["successful_operations"] = str(successful)
    if not row["failed_operations"]:
        row["failed_operations"] = str(recorded - successful)
    if not row["optional_missing"]:
        row["optional_missing"] = str(
            sum(status == "missing_optional" for status in statuses)
        )
    if not row["probed_operations"]:
        row["probed_operations"] = fields.get("attempted_operations", "")
    row["receipt"] = str(run_dir.relative_to(archive_dir))
    if (
        "rsync_attempted_operations" in fields
        and "rsync_metrics_unknown_operations" in fields
    ):
        try:
            attempted = int(fields["rsync_attempted_operations"])
            unknown = int(fields["rsync_metrics_unknown_operations"])
        except ValueError:
            row["metrics_provenance"] = "unknown"
        else:
            row["metrics_provenance"] = _metrics_provenance(attempted, unknown)
    else:
        row["metrics_provenance"] = "unknown"
    _seal_recovered_receipt(run_dir)
    return row


def _history_rows_from_receipts(
    archive_dir: Path, runs_dir: Path
) -> dict[str, dict[str, str]]:
    rows: dict[str, dict[str, str]] = {}
    for run_dir in sorted(runs_dir.iterdir()):
        if run_dir.is_symlink() or not run_dir.is_dir():
            continue
        row = _history_row_from_receipt(archive_dir, run_dir)
        if row is not None:
            rows[row["run_id"]] = row
    return rows


def _append_history_rows(history_path: Path, rows: Sequence[dict[str, str]]) -> None:
    if (
        not rows
        and os.path.lexists(history_path)
        and history_path.stat().st_size > 0
    ):
        return
    validate_history_file(history_path)
    flags = os.O_WRONLY | os.O_APPEND | os.O_CREAT
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(history_path, flags, 0o600)
    with os.fdopen(descriptor, "a", encoding="utf-8", newline="") as handle:
        if os.fstat(handle.fileno()).st_size == 0:
            _write_tsv(handle, HISTORY_COLUMNS)
        for row in rows:
            _write_schema_row(
                handle, HISTORY_COLUMNS, (row[column] for column in HISTORY_COLUMNS)
            )
        os.fsync(handle.fileno())


def _replace_legacy_history(
    runs_dir: Path, history_path: Path, rows: Sequence[dict[str, str]]
) -> None:
    temporary = runs_dir / f".history-migration-{os.getpid()}.tsv"
    output = _open_exclusive_text(temporary)
    try:
        _write_tsv(output, HISTORY_COLUMNS)
        for row in rows:
            _write_schema_row(
                output, HISTORY_COLUMNS, (row[column] for column in HISTORY_COLUMNS)
            )
        output.flush()
        os.fsync(output.fileno())
    finally:
        output.close()
    stamp = dt.datetime.now(tz=dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    backup = runs_dir / f"history.legacy-{stamp}-{os.getpid()}.tsv"
    os.link(history_path, backup)
    os.replace(temporary, history_path)
    os.chmod(backup, 0o400, follow_symlinks=False)
    directory_descriptor = os.open(runs_dir, os.O_RDONLY)
    try:
        os.fsync(directory_descriptor)
    finally:
        os.close(directory_descriptor)


def backfill_history(archive_dir: Path, runs_dir: Path) -> tuple[int, bool]:
    history_path = runs_dir / "history.tsv"
    receipt_rows = _history_rows_from_receipts(archive_dir, runs_dir)
    migrated = False
    existing_rows: list[dict[str, str]] = []
    header: tuple[str, ...] = ()
    if os.path.lexists(history_path):
        metadata = history_path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise SafetyError(f"history path is not a regular file: {history_path}")
        header, parsed_rows = _read_tsv_table(history_path)
        if header == HISTORY_COLUMNS:
            existing_rows = parsed_rows
        elif header == LEGACY_HISTORY_COLUMNS:
            existing_rows = [_legacy_history_row(row) for row in parsed_rows]
            migrated = True
        elif header:
            raise SafetyError(
                f"unsupported history schema in {history_path}; refusing migration"
            )
    seen: set[str] = set()
    merged: list[dict[str, str]] = []
    for existing in existing_rows:
        run_id = existing["run_id"]
        if not run_id or run_id in seen:
            raise SafetyError(f"duplicate or empty run_id in history: {run_id!r}")
        seen.add(run_id)
        receipt = receipt_rows.get(run_id)
        if migrated and receipt is not None:
            receipt["legacy_transferred_file_bytes_approx"] = existing[
                "legacy_transferred_file_bytes_approx"
            ]
            if receipt["legacy_transferred_file_bytes_approx"]:
                receipt["metrics_provenance"] = "legacy_approximate"
            for column in HISTORY_COLUMNS:
                if not receipt[column]:
                    receipt[column] = existing[column]
            merged.append(receipt)
        else:
            merged.append(existing)
    additions = [
        receipt_rows[run_id]
        for run_id in sorted(receipt_rows)
        if run_id not in seen
    ]
    if migrated:
        _replace_legacy_history(runs_dir, history_path, (*merged, *additions))
    elif not os.path.lexists(history_path):
        _append_history_rows(history_path, (*merged, *additions))
    else:
        validate_history_file(history_path)
        _append_history_rows(history_path, additions)
    return len(additions), migrated


def print_history(archive_dir: Path) -> None:
    runs_dir = archive_dir / "_fetch_runs"
    if not os.path.lexists(runs_dir):
        print("\t".join(HISTORY_COLUMNS))
        return
    validate_safe_directory_chain(archive_dir, runs_dir, leaf_may_be_missing=False)
    history_path = runs_dir / "history.tsv"
    if not os.path.lexists(history_path):
        print("\t".join(HISTORY_COLUMNS))
        return
    metadata = history_path.lstat()
    if (
        stat.S_ISLNK(metadata.st_mode)
        or not stat.S_ISREG(metadata.st_mode)
        or metadata.st_uid != os.getuid()
    ):
        raise SafetyError(f"history path is not a safe owned regular file: {history_path}")
    header, _ = _read_tsv_table(history_path)
    if header == LEGACY_HISTORY_COLUMNS:
        print(
            "warning: legacy history schema; run --backfill-history to migrate "
            "and add missing receipts",
            file=sys.stderr,
        )
    elif header != HISTORY_COLUMNS:
        raise SafetyError(f"unsupported history schema in {history_path}")
    sys.stdout.write(history_path.read_text(encoding="utf-8"))


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
    except ValueError:
        return False
    return True


def _validate_existing_directory(path: Path, label: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise SafetyError(f"cannot inspect {label} {path}: {error}") from error
    if stat.S_ISLNK(metadata.st_mode):
        raise SafetyError(f"{label} may not be a symlink: {path}")
    if not stat.S_ISDIR(metadata.st_mode):
        raise SafetyError(f"{label} must be a directory: {path}")
    return metadata


def validate_archive_directory(archive_dir: Path) -> None:
    metadata = _validate_existing_directory(archive_dir, "archive directory")
    if metadata.st_uid != os.getuid():
        raise SafetyError(f"archive directory must be owned by uid {os.getuid()}: {archive_dir}")
    permissions = stat.S_IMODE(metadata.st_mode)
    if permissions & 0o077:
        raise SafetyError(
            f"archive directory must be private (0700 or stricter), found "
            f"{permissions:04o}: {archive_dir}"
        )
    if archive_dir == Path(archive_dir.anchor):
        raise SafetyError("the filesystem root may not be used as an archive directory")


def validate_safe_directory_chain(
    archive_dir: Path, target: Path, *, leaf_may_be_missing: bool
) -> None:
    if not _is_within(target, archive_dir):
        raise SafetyError(f"destination escapes archive directory: {target}")
    current = archive_dir
    missing = False
    for component in target.relative_to(archive_dir).parts:
        _validate_component("destination component", component)
        current = current / component
        if missing:
            continue
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            missing = True
            continue
        except OSError as error:
            raise SafetyError(f"cannot inspect destination component {current}: {error}") from error
        if stat.S_ISLNK(metadata.st_mode):
            raise SafetyError(f"destination path contains a symlink: {current}")
        if not stat.S_ISDIR(metadata.st_mode):
            raise SafetyError(f"destination component is not a directory: {current}")
        try:
            resolved = current.resolve(strict=True)
        except OSError as error:
            raise SafetyError(f"cannot resolve destination component {current}: {error}") from error
        if not _is_within(resolved, archive_dir):
            raise SafetyError(f"resolved destination escapes archive directory: {current}")
    if missing and not leaf_may_be_missing:
        raise SafetyError(f"required destination directory does not exist: {target}")


def ensure_safe_directory(archive_dir: Path, target: Path, mode: int = 0o700) -> None:
    validate_safe_directory_chain(archive_dir, target, leaf_may_be_missing=True)
    current = archive_dir
    for component in target.relative_to(archive_dir).parts:
        current = current / component
        try:
            os.mkdir(current, mode)
        except FileExistsError:
            metadata = current.lstat()
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
                raise SafetyError(f"unsafe directory component appeared: {current}")
        except OSError as error:
            raise SafetyError(f"cannot create private directory {current}: {error}") from error
    validate_safe_directory_chain(archive_dir, target, leaf_may_be_missing=False)
    os.chmod(target, mode, follow_symlinks=False)


def validate_operation_destinations(
    archive_dir: Path, operations: Sequence[Operation]
) -> None:
    for operation in operations:
        validate_safe_directory_chain(
            archive_dir, operation.destination, leaf_may_be_missing=True
        )


def disk_snapshot(path: Path) -> DiskSnapshot:
    usage = shutil.disk_usage(path)
    return DiskSnapshot(total=usage.total, used=usage.used, free=usage.free)


class FetchLock:
    """Non-blocking Linux flock held for the entire receipt transaction."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.handle: TextIO | None = None

    def __enter__(self) -> FetchLock:
        flags = os.O_RDWR | os.O_CREAT
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            descriptor = os.open(self.path, flags, 0o600)
        except OSError as error:
            raise SafetyError(f"cannot safely open fetch lock {self.path}: {error}") from error
        handle = os.fdopen(descriptor, "a+", encoding="utf-8")
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            handle.close()
            raise LockBusy(f"another fetch is already running (lock: {self.path})") from error
        self.handle = handle
        return self

    def __exit__(self, exc_type: object, exc_value: object, traceback: object) -> None:
        if self.handle is not None:
            fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
            self.handle.close()
            self.handle = None


class RunLogger:
    """Tee human-readable progress to the terminal and the private run log."""

    def __init__(self, path: Path) -> None:
        self.handle = _open_exclusive_text(path)

    def line(self, message: str = "", *, error: bool = False) -> None:
        stream = sys.stderr if error else sys.stdout
        print(message, file=stream, flush=True)
        print(message, file=self.handle, flush=True)

    def raw(self, text: str) -> None:
        sys.stdout.write(text)
        sys.stdout.flush()
        self.handle.write(text)
        self.handle.flush()

    def close(self) -> None:
        self.handle.flush()
        os.fsync(self.handle.fileno())
        self.handle.close()


def _write_exclusive_bytes(destination: Path, contents: bytes) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor = os.open(destination, flags, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
    except BaseException:
        try:
            os.close(descriptor)
        except OSError:
            pass
        raise


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def _seal_receipt_tree(run_dir: Path) -> None:
    paths = sorted(run_dir.rglob("*"), key=lambda path: len(path.parts), reverse=True)
    for path in paths:
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            raise SafetyError(f"receipt unexpectedly contains a symlink: {path}")
        if stat.S_ISDIR(metadata.st_mode):
            os.chmod(path, 0o500, follow_symlinks=False)
        elif stat.S_ISREG(metadata.st_mode):
            os.chmod(path, 0o400, follow_symlinks=False)
        else:
            raise SafetyError(f"receipt contains an unsupported file type: {path}")
    os.chmod(run_dir, 0o500, follow_symlinks=False)


class Receipt:
    """Append while a run is live, then seal every receipt file read-only."""

    def __init__(
        self,
        archive_dir: Path,
        run_id: str,
        options: Options,
        expected_operations: int,
        configuration_snapshot: ConfigurationSnapshot,
        fetcher_file: Path,
        started_utc: str,
    ) -> None:
        self.archive_dir = archive_dir
        self.runs_dir = archive_dir / "_fetch_runs"
        self.run_id = run_id
        self.run_dir = self.runs_dir / run_id
        os.mkdir(self.run_dir, 0o700)
        try:
            with ExitStack() as cleanup:
                logger = RunLogger(self.run_dir / "fetch.log")
                cleanup.callback(logger.close)
                results = _open_exclusive_text(self.run_dir / "results.tsv")
                cleanup.callback(results.close)
                warnings = _open_exclusive_text(self.run_dir / "warnings.jsonl")
                cleanup.callback(warnings.close)
                run_metadata = _open_exclusive_text(self.run_dir / "run.tsv")
                cleanup.callback(run_metadata.close)
                _write_tsv(results, RESULT_COLUMNS)
                _write_exclusive_bytes(
                    self.run_dir / "machines.tsv",
                    configuration_snapshot.machines_bytes,
                )
                _write_exclusive_bytes(
                    self.run_dir / "log_roots.tsv",
                    configuration_snapshot.roots_bytes,
                )
                checksums = _open_exclusive_text(self.run_dir / "fetcher.sha256")
                try:
                    _write_tsv(checksums, ("sha256", "file"))
                    _write_tsv(checksums, (_sha256(fetcher_file), fetcher_file.name))
                    launcher = archive_dir / "fetch_agent_logs.sh"
                    if launcher.is_file() and not launcher.is_symlink():
                        _write_tsv(checksums, (_sha256(launcher), launcher.name))
                    checksums.flush()
                    os.fsync(checksums.fileno())
                finally:
                    checksums.close()
                _write_tsv(run_metadata, ("field", "value"))
                _write_tsv(run_metadata, ("run_id", run_id))
                _write_tsv(run_metadata, ("started_utc", started_utc))
                _write_tsv(run_metadata, ("mode", options.mode.value))
                _write_tsv(run_metadata, ("executing_host", socket.getfqdn()))
                _write_tsv(run_metadata, ("archive_dir", archive_dir))
                _write_tsv(run_metadata, ("expected_operations", expected_operations))
                _write_tsv(run_metadata, ("min_free_bytes", options.min_free_bytes))
                cleanup.pop_all()
        except BaseException as error:
            try:
                failure = _open_exclusive_text(self.run_dir / "initialization_error.txt")
                try:
                    failure.write(f"{type(error).__name__}: {error}\n")
                    failure.flush()
                    os.fsync(failure.fileno())
                finally:
                    failure.close()
            except OSError:
                pass
            _seal_receipt_tree(self.run_dir)
            raise
        self.logger = logger
        self.results = results
        self.warnings = warnings
        self.run_metadata = run_metadata
        self.closed = False

    def record_result(self, result: OperationResult) -> None:
        _write_schema_row(
            self.results,
            RESULT_COLUMNS,
            (
                result.machine,
                result.archive_name,
                result.requirement,
                result.status,
                result.exit_code,
                result.started_utc,
                result.finished_utc,
                f"{result.duration_seconds:.6f}",
                result.source_estimated_bytes,
                result.manifest_entries,
                result.manifest_regular_bytes,
                result.destination_before_bytes,
                result.destination_after_bytes,
                result.rsync_total_files,
                result.rsync_transferred_files,
                result.rsync_total_file_size,
                result.rsync_transferred_file_size,
                result.rsync_literal_data,
                result.rsync_matched_data,
                result.rsync_bytes_sent,
                result.rsync_bytes_received,
                result.retained_destination_entries,
                result.retained_destination_bytes,
                result.stale_sqlite_sidecars,
                result.retained_sqlite_databases,
                result.rewrite_warnings,
                result.warning_count,
                result.detail,
            ),
        )

    def record_warning(self, warning: WarningRecord) -> None:
        record: dict[str, str | int | None] = {
            "timestamp_utc": warning.timestamp_utc,
            "machine": warning.machine,
            "archive_name": warning.archive_name,
            "code": warning.code,
            "path": warning.path,
            "detail": warning.detail,
            "source_size": warning.source_size,
            "destination_size": warning.destination_size,
        }
        self.warnings.write(json.dumps(record, sort_keys=True, ensure_ascii=True) + "\n")
        self.warnings.flush()

    def manifest_path(self, operation: Operation) -> Path:
        manifests = self.run_dir / "manifests"
        machine_dir = manifests / operation.machine.short_name
        manifests.mkdir(mode=0o700, exist_ok=True)
        machine_dir.mkdir(mode=0o700, exist_ok=True)
        return machine_dir / f"{operation.root.archive_name}.jsonl"

    def finalize(self, summary: RunSummary) -> None:
        for field, value in (
            ("finished_utc", summary.finished_utc),
            ("status", summary.status),
            ("exit_code", summary.exit_code),
            ("duration_seconds", f"{summary.duration_seconds:.6f}"),
            ("recorded_operations", summary.recorded_operations),
            ("successful_operations", summary.successful_operations),
            ("failed_operations", summary.failed_operations),
            ("optional_missing", summary.optional_missing),
            ("probed_operations", summary.probed_operations),
            ("rsync_attempted_operations", summary.rsync_attempted_operations),
            (
                "rsync_metrics_complete_operations",
                summary.rsync_metrics_complete_operations,
            ),
            (
                "rsync_metrics_unknown_operations",
                summary.rsync_metrics_unknown_operations,
            ),
            ("manifest_entries", summary.manifest_entries),
            ("manifest_regular_bytes", summary.manifest_regular_bytes),
            ("rsync_transferred_files", summary.rsync_transferred_files),
            ("rsync_transferred_file_size", summary.rsync_transferred_file_size),
            ("rsync_literal_data", summary.rsync_literal_data),
            ("rsync_matched_data", summary.rsync_matched_data),
            ("rsync_bytes_sent", summary.rsync_bytes_sent),
            ("rsync_bytes_received", summary.rsync_bytes_received),
            ("retained_destination_entries", summary.retained_destination_entries),
            ("retained_destination_bytes", summary.retained_destination_bytes),
            ("stale_sqlite_sidecars", summary.stale_sqlite_sidecars),
            ("retained_sqlite_databases", summary.retained_sqlite_databases),
            ("rewrite_warnings", summary.rewrite_warnings),
            ("warning_count", summary.warning_count),
            ("disk_total_bytes", summary.disk_total_bytes),
            ("disk_free_before_bytes", summary.disk_free_before_bytes),
            ("disk_free_after_bytes", summary.disk_free_after_bytes),
        ):
            _write_tsv(self.run_metadata, (field, value))
        self.logger.line()
        self.logger.line(
            f"Run {self.run_id}: {summary.status} (exit {summary.exit_code})"
        )
        self.logger.line(f"Metadata: {self.run_dir}")
        for handle in (self.results, self.warnings, self.run_metadata):
            handle.flush()
            os.fsync(handle.fileno())
            handle.close()
        self.logger.close()
        self.closed = True
        self._seal()
        self._append_history(summary)
        self._update_latest()

    def _append_history(self, summary: RunSummary) -> None:
        history_path = self.runs_dir / "history.tsv"
        validate_history_file(history_path)
        if history_path.exists() and history_path.stat().st_size:
            _, rows = _read_tsv_table(history_path)
            if any(row["run_id"] == summary.run_id for row in rows):
                raise SafetyError(f"history already contains run_id {summary.run_id}")
        flags = os.O_WRONLY | os.O_APPEND | os.O_CREAT
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(history_path, flags, 0o600)
        with os.fdopen(descriptor, "a", encoding="utf-8", newline="") as handle:
            if os.fstat(handle.fileno()).st_size == 0:
                _write_tsv(handle, HISTORY_COLUMNS)
            _write_schema_row(
                handle,
                HISTORY_COLUMNS,
                (
                    summary.run_id,
                    summary.started_utc,
                    summary.finished_utc,
                    f"{summary.duration_seconds:.6f}",
                    summary.mode,
                    summary.status,
                    summary.exit_code,
                    summary.expected_operations,
                    summary.recorded_operations,
                    summary.successful_operations,
                    summary.failed_operations,
                    summary.optional_missing,
                    summary.probed_operations,
                    summary.rsync_attempted_operations,
                    summary.rsync_metrics_complete_operations,
                    summary.rsync_metrics_unknown_operations,
                    summary.manifest_entries,
                    summary.manifest_regular_bytes,
                    summary.rsync_transferred_files,
                    summary.rsync_transferred_file_size,
                    summary.rsync_literal_data,
                    summary.rsync_matched_data,
                    summary.rsync_bytes_sent,
                    summary.rsync_bytes_received,
                    "",
                    _metrics_provenance(
                        summary.rsync_attempted_operations,
                        summary.rsync_metrics_unknown_operations,
                    ),
                    summary.retained_destination_entries,
                    summary.retained_destination_bytes,
                    summary.stale_sqlite_sidecars,
                    summary.retained_sqlite_databases,
                    summary.rewrite_warnings,
                    summary.warning_count,
                    summary.disk_total_bytes,
                    summary.disk_free_before_bytes,
                    summary.disk_free_after_bytes,
                    self.run_dir.relative_to(self.archive_dir),
                ),
            )
            os.fsync(handle.fileno())

    def _seal(self) -> None:
        _seal_receipt_tree(self.run_dir)

    def _update_latest(self) -> None:
        latest = self.runs_dir / "latest"
        if os.path.lexists(latest) and not latest.is_symlink():
            raise SafetyError(f"refusing to replace non-symlink latest path: {latest}")
        temporary = self.runs_dir / f".latest-{self.run_id}"
        os.symlink(self.run_id, temporary)
        os.replace(temporary, latest)


def _ssh_command(host: str, remote_command: str) -> list[str]:
    return ["ssh", *SSH_OPTIONS, "--", host, remote_command]


def _run_bounded(
    command: Sequence[str], *, timeout_s: float
) -> subprocess.CompletedProcess[str]:
    """Run a child process under a wall-clock ceiling, reporting a timeout as a normal failure.

    A ``TimeoutExpired`` is converted into a nonzero ``CompletedProcess`` rather than propagated,
    because every caller here already has a path for "this probe failed, and here is the stderr" --
    and turning a hang into that path is the entire point. Raising would replace an infinite wait
    with a traceback, which is better but still not an answer the operator can act on.

    The exit code is 124, matching :manpage:`timeout(1)`, so a reader who greps for it finds the
    convention rather than a number this file invented.
    """

    try:
        return subprocess.run(
            list(command),
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_s,
            env={**os.environ, "LC_ALL": "C"},
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(
            list(command), TIMED_OUT, "", _timeout_detail(command, timeout_s)
        )


def _run_bounded_bytes(
    command: Sequence[str], *, timeout_s: float
) -> subprocess.CompletedProcess[bytes]:
    """:func:`_run_bounded` for a caller that needs raw bytes.

    Two functions rather than one with a ``text`` switch, because the return type genuinely
    differs between them and a single signature would have to say ``CompletedProcess[Any]`` --
    which this repository's type contract forbids, and rightly: every caller here does know
    which of the two it wants.
    """

    try:
        return subprocess.run(
            list(command),
            capture_output=True,
            check=False,
            timeout=timeout_s,
            env={**os.environ, "LC_ALL": "C"},
        )
    except subprocess.TimeoutExpired:
        return subprocess.CompletedProcess(
            list(command), TIMED_OUT, b"", _timeout_detail(command, timeout_s).encode()
        )


def _timeout_detail(command: Sequence[str], timeout_s: float) -> str:
    """The one sentence a timed-out child gets to explain itself."""

    return (
        f"timed out after {timeout_s:.0f}s: {shlex.join(command)}. "
        "The connection was established or never refused, so this is a stall rather than a "
        "refusal -- most often an SSH agent that holds the connection open without ever "
        "authenticating."
    )


def _probe_remote_command(
    source: PurePosixPath, home: PurePosixPath, estimate_size: bool
) -> str:
    quoted = shlex.quote(str(source))
    quoted_home = shlex.quote(str(home))
    pieces = [
        f"source_path={quoted}",
        f"home_path={quoted_home}",
        'if ! test -d "$home_path"; then exit 46; fi',
        'if ! test -e "$source_path"; then exit 44; fi',
        'if ! test -d "$source_path"; then exit 45; fi',
        'if ! test -r "$source_path" || ! test -x "$source_path"; then exit 46; fi',
        'resolved_home=$(readlink -f -- "$home_path") || exit 46',
        'resolved_source=$(readlink -f -- "$source_path") || exit 46',
        'home_prefix=${resolved_home%/}/',
        'case "$resolved_source/" in "$home_prefix"*) ;; *) exit 48;; esac',
    ]
    if estimate_size:
        pieces.extend(
            [
                'size_line=$(du -sbl -- "$resolved_source" 2>/dev/null) || exit 47',
                'set -- $size_line',
                'case "$1" in ""|*[!0-9]*) exit 47;; esac',
                'printf "%s\\t%s\\n" "$1" "$resolved_source"',
            ]
        )
    else:
        pieces.append('printf "%s\\n" "$resolved_source"')
    return "; ".join(pieces)


def _probe_kind_from_remote_exit(exit_code: int) -> ProbeKind:
    if exit_code == REMOTE_MISSING:
        return ProbeKind.MISSING
    if exit_code == REMOTE_NOT_DIRECTORY:
        return ProbeKind.NOT_DIRECTORY
    if exit_code == REMOTE_INACCESSIBLE:
        return ProbeKind.INACCESSIBLE
    if exit_code == REMOTE_OUTSIDE_HOME:
        return ProbeKind.OUTSIDE_HOME
    return ProbeKind.FAILED


def probe_source(operation: Operation, *, estimate_size: bool) -> ProbeObservation:
    started_utc = utc_now()
    began = time.monotonic()
    kind = ProbeKind.PRESENT
    exit_code = 0
    estimated_bytes: int | None = None
    resolved_source: str | None = None
    detail = "source directory is present"
    if operation.local:
        source_path = Path(str(operation.source))
        try:
            home_resolved = Path(str(operation.machine.home)).resolve(strict=True)
            source_resolved = source_path.resolve(strict=True)
        except FileNotFoundError:
            kind = ProbeKind.MISSING
            exit_code = REMOTE_MISSING
            detail = "source path does not exist"
        except PermissionError:
            kind = ProbeKind.INACCESSIBLE
            exit_code = REMOTE_INACCESSIBLE
            detail = "source path is not accessible"
        except OSError as error:
            kind = ProbeKind.FAILED
            exit_code = 1
            detail = f"source stat failed: {error}"
        else:
            if not _is_within(source_resolved, home_resolved):
                kind = ProbeKind.OUTSIDE_HOME
                exit_code = REMOTE_OUTSIDE_HOME
                detail = "source resolves outside the configured machine home"
            elif not source_resolved.is_dir():
                kind = ProbeKind.NOT_DIRECTORY
                exit_code = REMOTE_NOT_DIRECTORY
                detail = "source path exists but is not a directory"
            elif not os.access(source_resolved, os.R_OK | os.X_OK):
                kind = ProbeKind.INACCESSIBLE
                exit_code = REMOTE_INACCESSIBLE
                detail = "source directory is not readable/searchable"
            elif estimate_size:
                resolved_source = str(source_resolved)
                completed = _run_bounded(
                    ["du", "-sbl", "--", str(source_resolved)],
                    timeout_s=PROBE_TIMEOUT_S,
                )
                if completed.returncode != 0:
                    kind = ProbeKind.FAILED
                    exit_code = REMOTE_SIZE_FAILED
                    detail = "source size probe failed: " + completed.stderr.strip()
                else:
                    first = completed.stdout.split(maxsplit=1)[0] if completed.stdout else ""
                    try:
                        estimated_bytes = int(first)
                    except ValueError:
                        kind = ProbeKind.FAILED
                        exit_code = REMOTE_SIZE_FAILED
                        detail = f"source size probe returned invalid output: {first!r}"
            else:
                resolved_source = str(source_resolved)
    else:
        completed = _run_bounded(
            _ssh_command(
                operation.machine.host,
                _probe_remote_command(
                    operation.source, operation.machine.home, estimate_size
                ),
            ),
            timeout_s=PROBE_TIMEOUT_S,
        )
        exit_code = completed.returncode
        if exit_code == 0:
            if estimate_size:
                fields = completed.stdout.rstrip("\r\n").split("\t", maxsplit=1)
                value = fields[0] if fields else ""
                try:
                    estimated_bytes = int(value)
                except ValueError:
                    kind = ProbeKind.FAILED
                    exit_code = REMOTE_SIZE_FAILED
                    detail = f"remote size probe returned invalid output: {value!r}"
                else:
                    if len(fields) != 2 or not fields[1].startswith("/"):
                        kind = ProbeKind.FAILED
                        exit_code = REMOTE_SIZE_FAILED
                        detail = "remote source probe did not return a canonical path"
                    else:
                        resolved_source = fields[1]
            else:
                canonical = completed.stdout.rstrip("\r\n")
                if not canonical.startswith("/"):
                    kind = ProbeKind.FAILED
                    exit_code = REMOTE_INACCESSIBLE
                    detail = "remote source probe did not return a canonical path"
                else:
                    resolved_source = canonical
                    detail = "remote source directory is present"
        else:
            kind = _probe_kind_from_remote_exit(exit_code)
            stderr = completed.stderr.strip()
            if stderr:
                detail = stderr
            elif kind is ProbeKind.OUTSIDE_HOME:
                detail = "remote source resolves outside the configured machine home"
            else:
                detail = f"remote source probe exited {exit_code}"
    finished_utc = utc_now()
    return ProbeObservation(
        kind=kind,
        exit_code=exit_code,
        estimated_bytes=estimated_bytes,
        resolved_source=resolved_source,
        detail=detail,
        started_utc=started_utc,
        finished_utc=finished_utc,
        duration_seconds=time.monotonic() - began,
    )


def _entry_kind(mode: int) -> EntryKind:
    if stat.S_ISDIR(mode):
        return EntryKind.DIRECTORY
    if stat.S_ISREG(mode):
        return EntryKind.FILE
    if stat.S_ISLNK(mode):
        return EntryKind.SYMLINK
    if stat.S_ISBLK(mode):
        return EntryKind.BLOCK
    if stat.S_ISCHR(mode):
        return EntryKind.CHARACTER
    if stat.S_ISFIFO(mode):
        return EntryKind.FIFO
    if stat.S_ISSOCK(mode):
        return EntryKind.SOCKET
    return EntryKind.OTHER


def scan_local_tree(root: Path) -> ManifestScan:
    """Observe one local tree without following symlinks below its root."""
    try:
        root_metadata = root.stat()
    except FileNotFoundError as error:
        raise ManifestSourceMissing(f"source disappeared before manifest: {root}") from error
    except OSError as error:
        raise ManifestError(f"cannot inspect manifest root {root}: {error}") from error
    if not stat.S_ISDIR(root_metadata.st_mode):
        raise ManifestError(f"manifest root is no longer a directory: {root}")
    entries: list[ManifestEntry] = [
        ManifestEntry(
            path=".",
            kind=EntryKind.DIRECTORY,
            size=root_metadata.st_size,
            mtime_ns=root_metadata.st_mtime_ns,
            link_target=None,
        )
    ]
    notices: list[str] = []
    pending: list[tuple[Path, PurePosixPath]] = [(root, PurePosixPath("."))]
    while pending:
        directory, relative_directory = pending.pop()
        try:
            with os.scandir(directory) as iterator:
                children = sorted(iterator, key=lambda child: child.name)
        except FileNotFoundError:
            if directory == root:
                raise ManifestSourceMissing(
                    f"source disappeared during manifest: {root}"
                )
            notices.append(f"directory vanished during manifest: {relative_directory}")
            continue
        except OSError as error:
            raise ManifestError(f"cannot scan {directory}: {error}") from error
        for child in children:
            relative = (
                PurePosixPath(child.name)
                if relative_directory == PurePosixPath(".")
                else relative_directory / child.name
            )
            try:
                metadata = child.stat(follow_symlinks=False)
            except FileNotFoundError:
                notices.append(f"entry vanished during manifest: {relative}")
                continue
            except OSError as error:
                raise ManifestError(f"cannot stat {relative}: {error}") from error
            kind = _entry_kind(metadata.st_mode)
            link_target: str | None = None
            if kind is EntryKind.SYMLINK:
                try:
                    link_target = os.readlink(child.path)
                except FileNotFoundError:
                    notices.append(f"symlink vanished during manifest: {relative}")
                    continue
                except OSError as error:
                    raise ManifestError(f"cannot read symlink {relative}: {error}") from error
            entries.append(
                ManifestEntry(
                    path=str(relative),
                    kind=kind,
                    size=metadata.st_size,
                    mtime_ns=metadata.st_mtime_ns,
                    link_target=link_target,
                )
            )
            if kind is EntryKind.DIRECTORY:
                pending.append((Path(child.path), relative))
    entries.sort(key=lambda entry: entry.path)
    return ManifestScan(entries=tuple(entries), notices=tuple(notices))


def _find_kind(code: str) -> EntryKind:
    mapping = {
        "d": EntryKind.DIRECTORY,
        "f": EntryKind.FILE,
        "l": EntryKind.SYMLINK,
        "b": EntryKind.BLOCK,
        "c": EntryKind.CHARACTER,
        "p": EntryKind.FIFO,
        "s": EntryKind.SOCKET,
    }
    return mapping.get(code, EntryKind.OTHER)


def _parse_find_mtime_ns(value: str) -> int:
    negative = value.startswith("-")
    unsigned = value[1:] if negative else value
    seconds_text, separator, fraction_text = unsigned.partition(".")
    if not seconds_text.isdigit() or (separator and not fraction_text.isdigit()):
        raise ManifestError(f"remote find returned invalid mtime: {value!r}")
    nanoseconds = int(seconds_text) * 1_000_000_000
    if separator:
        nanoseconds += int((fraction_text + "000000000")[:9])
    return -nanoseconds if negative else nanoseconds


def _remote_manifest_command(
    source: PurePosixPath, home: PurePosixPath
) -> str:
    quoted = shlex.quote(str(source))
    quoted_home = shlex.quote(str(home))
    return "; ".join(
        (
            f"source_path={quoted}",
            f"home_path={quoted_home}",
            'if ! test -d "$home_path"; then exit 46; fi',
            'if ! test -e "$source_path"; then exit 44; fi',
            'if ! test -d "$source_path"; then exit 45; fi',
            'if ! test -r "$source_path" || ! test -x "$source_path"; then exit 46; fi',
            'resolved_home=$(readlink -f -- "$home_path") || exit 46',
            'resolved_source=$(readlink -f -- "$source_path") || exit 46',
            'home_prefix=${resolved_home%/}/',
            'case "$resolved_source/" in "$home_prefix"*) ;; *) exit 48;; esac',
            'cd -- "$source_path" || exit 46',
            r"find . -printf '%P\0%y\0%s\0%T@\0%l\0'",
        )
    )


def _remote_manifest_stderr(stderr: bytes) -> str:
    """Remove the exact known SSH banner line while preserving diagnostics."""
    lines = os.fsdecode(stderr).splitlines()
    return "\n".join(
        line
        for line in lines
        if line not in IGNORED_REMOTE_MANIFEST_STDERR_LINES
    ).strip()


def scan_remote_tree(
    operation: Operation, resolved_source: PurePosixPath
) -> ManifestScan:
    completed = _run_bounded_bytes(
        _ssh_command(
            operation.machine.host,
            _remote_manifest_command(resolved_source, operation.machine.home),
        ),
        timeout_s=MANIFEST_TIMEOUT_S,
    )
    if completed.returncode != 0:
        stderr = _remote_manifest_stderr(completed.stderr)
        probe_label = _probe_kind_from_remote_exit(completed.returncode).value
        detail = stderr or f"remote manifest exited {completed.returncode}"
        if completed.returncode == REMOTE_MISSING:
            raise ManifestSourceMissing(f"remote manifest {probe_label}: {detail}")
        raise ManifestError(f"remote manifest {probe_label}: {detail}")
    fields = completed.stdout.split(b"\0")
    if fields and fields[-1] == b"":
        fields.pop()
    if len(fields) % 5 != 0:
        raise ManifestError(
            f"remote manifest returned {len(fields)} fields, expected a multiple of 5"
        )
    entries: list[ManifestEntry] = []
    seen: set[str] = set()
    for index in range(0, len(fields), 5):
        relative = os.fsdecode(fields[index]) or "."
        kind_code = fields[index + 1].decode("ascii", errors="strict")
        size_text = fields[index + 2].decode("ascii", errors="strict")
        mtime_text = fields[index + 3].decode("ascii", errors="strict")
        link_target_text = os.fsdecode(fields[index + 4])
        try:
            size = int(size_text)
        except ValueError as error:
            raise ManifestError(
                f"remote find returned invalid size for {relative}: {size_text!r}"
            ) from error
        if relative in seen:
            raise ManifestError(f"remote manifest returned duplicate path: {relative}")
        if relative != "." and (
            PurePosixPath(relative).is_absolute()
            or ".." in PurePosixPath(relative).parts
        ):
            raise ManifestError(f"remote manifest returned unsafe path: {relative}")
        seen.add(relative)
        kind = _find_kind(kind_code)
        entries.append(
            ManifestEntry(
                path=relative,
                kind=kind,
                size=size,
                mtime_ns=_parse_find_mtime_ns(mtime_text),
                link_target=link_target_text if kind is EntryKind.SYMLINK else None,
            )
        )
    entries.sort(key=lambda entry: entry.path)
    stderr_notice = _remote_manifest_stderr(completed.stderr)
    notices = (f"remote find reported: {stderr_notice}",) if stderr_notice else ()
    return ManifestScan(entries=tuple(entries), notices=notices)


def scan_source_tree(operation: Operation, resolved_source: str) -> ManifestScan:
    if operation.local:
        return scan_local_tree(Path(resolved_source))
    return scan_remote_tree(operation, PurePosixPath(resolved_source))


def scan_destination_tree(destination: Path) -> ManifestScan:
    if not os.path.lexists(destination):
        return ManifestScan(entries=(), notices=())
    return scan_local_tree(destination)


def write_source_manifest(
    path: Path, operation: Operation, resolved_source: str, scan: ManifestScan
) -> None:
    handle = _open_exclusive_text(path)
    try:
        for entry in scan.entries:
            record: dict[str, str | int | None] = {
                "machine": operation.machine.short_name,
                "archive_name": operation.root.archive_name,
                "configured_source": str(operation.source),
                "resolved_source": resolved_source,
                "path": entry.path,
                "type": entry.kind.value,
                "size": entry.size,
                "mtime_ns": entry.mtime_ns,
                "link_target": entry.link_target,
            }
            handle.write(json.dumps(record, sort_keys=True, ensure_ascii=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    finally:
        handle.close()


def _is_sqlite_sidecar(path: str) -> bool:
    lowered = path.lower()
    return lowered.endswith(SQLITE_SIDECAR_SUFFIXES)


def _is_sqlite_database(path: str) -> bool:
    lowered = path.lower()
    return lowered.endswith(SQLITE_SUFFIXES)


def compare_source_and_destination(
    operation: Operation,
    source: ManifestScan,
    destination_before: ManifestScan,
    destination_after: ManifestScan,
    *,
    actual_fetch: bool,
) -> Comparison:
    source_entries = {entry.path: entry for entry in source.entries}
    before_entries = {entry.path: entry for entry in destination_before.entries}
    after_entries = {entry.path: entry for entry in destination_after.entries}
    warnings: list[WarningRecord] = []
    errors: list[str] = []
    rewrite_warnings = 0

    def warn(
        code: str,
        path: str,
        detail: str,
        source_size: int | None = None,
        destination_size: int | None = None,
    ) -> None:
        warnings.append(
            WarningRecord(
                timestamp_utc=utc_now(),
                machine=operation.machine.short_name,
                archive_name=operation.root.archive_name,
                code=code,
                path=path,
                detail=detail,
                source_size=source_size,
                destination_size=destination_size,
            )
        )

    for path, source_entry in source_entries.items():
        if path == ".":
            continue
        before_entry = before_entries.get(path)
        if (
            before_entry is not None
            and source_entry.kind is EntryKind.FILE
            and before_entry.kind is EntryKind.FILE
        ):
            if source_entry.size < before_entry.size:
                rewrite_warnings += 1
                warn(
                    "source_smaller_than_previous_destination",
                    path,
                    "source appears truncated or rewritten relative to the prior archive member",
                    source_entry.size,
                    before_entry.size,
                )
            if source_entry.mtime_ns < before_entry.mtime_ns:
                rewrite_warnings += 1
                warn(
                    "source_older_than_previous_destination",
                    path,
                    "source mtime is older than the prior archive member",
                    source_entry.size,
                    before_entry.size,
                )
        if _is_sqlite_sidecar(path):
            after_sidecar = after_entries.get(path)
            warn(
                "source_sqlite_sidecar_present",
                path,
                "source has a live SQLite WAL/SHM sidecar; validate a copied DB/WAL set",
                source_entry.size,
                after_sidecar.size if after_sidecar is not None else None,
            )
        if actual_fetch:
            after_entry = after_entries.get(path)
            if after_entry is None:
                errors.append(f"destination is missing source-manifest member: {path}")
            elif source_entry.kind is not after_entry.kind:
                errors.append(
                    f"destination type mismatch for {path}: "
                    f"source={source_entry.kind.value} destination={after_entry.kind.value}"
                )
            elif (
                source_entry.kind is EntryKind.SYMLINK
                and source_entry.link_target != after_entry.link_target
            ):
                errors.append(f"destination symlink target mismatch for {path}")
            elif (
                source_entry.kind is EntryKind.FILE
                and source_entry.size != after_entry.size
            ):
                warn(
                    "source_changed_during_transfer",
                    path,
                    "post-transfer source size differs from destination; source was likely active",
                    source_entry.size,
                    after_entry.size,
                )
            elif (
                source_entry.kind is EntryKind.FILE
                and source_entry.mtime_ns != after_entry.mtime_ns
            ):
                warn(
                    "source_mtime_differs_after_transfer",
                    path,
                    "post-transfer source mtime differs from destination; source may have changed",
                    source_entry.size,
                    after_entry.size,
                )

    retained_paths = set(after_entries).difference(source_entries)
    retained_paths.discard(".")
    retained_entries = len(retained_paths)
    retained_bytes = sum(
        after_entries[path].size
        for path in retained_paths
        if after_entries[path].kind is EntryKind.FILE
    )
    stale_sidecars = 0
    retained_databases = 0
    for path in sorted(retained_paths):
        entry = after_entries[path]
        if _is_sqlite_sidecar(path):
            stale_sidecars += 1
            warn(
                "stale_destination_sqlite_sidecar",
                path,
                "destination WAL/SHM is absent from the source manifest and was retained because deletion is disabled",
                None,
                entry.size,
            )
        elif _is_sqlite_database(path):
            retained_databases += 1
            warn(
                "retained_destination_sqlite_database",
                path,
                "destination database is absent from the source manifest and belongs to retained history",
                None,
                entry.size,
            )
    return Comparison(
        warnings=tuple(warnings),
        errors=tuple(errors),
        retained_entries=retained_entries,
        retained_bytes=retained_bytes,
        stale_sqlite_sidecars=stale_sidecars,
        retained_sqlite_databases=retained_databases,
        rewrite_warnings=rewrite_warnings,
    )


_METRIC_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("total_files", re.compile(r"^Number of files: ([0-9,]+)")),
    ("transferred_files", re.compile(r"^Number of regular files transferred: ([0-9,]+)")),
    ("total_file_size", re.compile(r"^Total file size: ([0-9,]+) bytes")),
    (
        "transferred_file_size",
        re.compile(r"^Total transferred file size: ([0-9,]+) bytes"),
    ),
    ("literal_data", re.compile(r"^Literal data: ([0-9,]+) bytes")),
    ("matched_data", re.compile(r"^Matched data: ([0-9,]+) bytes")),
    ("bytes_sent", re.compile(r"^Total bytes sent: ([0-9,]+)")),
    ("bytes_received", re.compile(r"^Total bytes received: ([0-9,]+)")),
)
_RSYNC_IO_PATTERN = re.compile(
    r"^sent ([0-9,]+) bytes\s+received ([0-9,]+) bytes(?:\s|$)"
)


def parse_rsync_metrics(output: str) -> RsyncMetrics:
    values: dict[str, int] = {}
    for line in output.replace("\r", "\n").splitlines():
        stripped = line.strip()
        io_match = _RSYNC_IO_PATTERN.search(stripped)
        if io_match is not None:
            values["bytes_sent"] = int(io_match.group(1).replace(",", ""))
            values["bytes_received"] = int(io_match.group(2).replace(",", ""))
        for name, pattern in _METRIC_PATTERNS:
            match = pattern.search(stripped)
            if match is not None:
                values[name] = int(match.group(1).replace(",", ""))
    return RsyncMetrics(
        total_files=values.get("total_files"),
        transferred_files=values.get("transferred_files"),
        total_file_size=values.get("total_file_size"),
        transferred_file_size=values.get("transferred_file_size"),
        literal_data=values.get("literal_data"),
        matched_data=values.get("matched_data"),
        bytes_sent=values.get("bytes_sent"),
        bytes_received=values.get("bytes_received"),
    )


def rsync_metrics_complete(metrics: RsyncMetrics) -> bool:
    return all(
        value is not None
        for value in (
            metrics.total_files,
            metrics.transferred_files,
            metrics.total_file_size,
            metrics.transferred_file_size,
            metrics.literal_data,
            metrics.matched_data,
            metrics.bytes_sent,
            metrics.bytes_received,
        )
    )


def build_rsync_command(
    operation: Operation,
    resolved_source: str,
    destination: Path,
    partial_directory: Path,
    mode: Mode,
) -> list[str]:
    source = (
        f"{resolved_source.rstrip('/')}/"
        if operation.local
        else f"{operation.machine.host}:{resolved_source.rstrip('/')}/"
    )
    command = [
        "rsync",
        "--archive",
        "--protect-args",
        "--partial",
        f"--partial-dir={partial_directory}",
        "--stats",
    ]
    if not operation.local:
        remote_shell = shlex.join(("ssh", *RSYNC_SSH_OPTIONS))
        command.append(f"--rsh={remote_shell}")
    if mode is Mode.DRY_RUN:
        command.extend(("--dry-run", "--info=stats2"))
    else:
        command.append("--info=progress2,stats2")
    command.extend(("--", source, f"{str(destination).rstrip('/')}/"))
    if any(argument == "--delete" or argument.startswith("--delete-") for argument in command):
        raise SafetyError("internal error: destructive rsync option was constructed")
    return command


def run_rsync(command: Sequence[str], logger: RunLogger) -> RsyncOutcome:
    logger.line(f"Command: {shlex.join(command)}")
    process = subprocess.Popen(
        list(command),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="backslashreplace",
        env={**os.environ, "LC_ALL": "C"},
    )
    if process.stdout is None:
        process.kill()
        raise FetchError("rsync stdout pipe was not created")
    chunks: list[str] = []
    try:
        while True:
            chunk = process.stdout.read(64 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
            logger.raw(chunk)
        exit_code = process.wait()
    except BaseException:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        raise
    finally:
        process.stdout.close()
    return RsyncOutcome(
        exit_code=exit_code,
        metrics=parse_rsync_metrics("".join(chunks)),
    )


def _agent_socket_answers(candidate: str) -> bool:
    """Whether *candidate* is an SSH agent that ANSWERS, not merely a socket file that exists.

    The distinction is the whole bug. ``stat`` on a forwarded agent socket whose session has ended
    still reports ``S_ISSOCK``: the inode outlives the listener. Accepting it on that evidence set
    ``SSH_AUTH_SOCK`` to a dead socket, printed "Using forwarded SSH agent", and left every
    subsequent ssh blocked in authentication -- past ``ConnectTimeout``, which had already been
    satisfied by a TCP connection that succeeded.

    So this asks the agent a question. ``ssh-add -l`` exits 0 with keys, 1 with none (a live agent
    either way), and 2 when it cannot reach one. Only "cannot reach" disqualifies a socket: an
    agent with no identities is still an agent, and rejecting it here would silently skip a
    perfectly good one over a condition ssh itself would report clearly.
    """

    try:
        if not stat.S_ISSOCK(Path(candidate).stat().st_mode):
            return False
    except OSError:
        return False
    if shutil.which("ssh-add") is None:
        # Cannot ask, so do not claim to know. Existence is the only evidence available, and it is
        # the evidence this function exists to distrust -- but refusing every socket on a host
        # without `ssh-add` would break fetching there entirely.
        return True
    try:
        probed = subprocess.run(
            ["ssh-add", "-l"],
            capture_output=True,
            text=True,
            check=False,
            timeout=AGENT_PROBE_TIMEOUT_S,
            env={**os.environ, "SSH_AUTH_SOCK": candidate},
        )
    except subprocess.TimeoutExpired:
        # A dead socket does not always refuse: it can accept the connection and never reply, so
        # even the liveness probe needs a ceiling. Measured against the real stale sockets on the
        # host this was written for, `ssh-add -l` produced no output and did not return.
        return False
    return probed.returncode in (0, 1)


def discover_forwarded_ssh_agent(logger: RunLogger) -> None:
    current = os.environ.get("SSH_AUTH_SOCK")
    if current is not None and _agent_socket_answers(current):
        return
    if shutil.which("tmux") is None:
        return
    completed = subprocess.run(
        ["tmux", "show-environment", "-g", "SSH_AUTH_SOCK"],
        capture_output=True,
        text=True,
        check=False,
    )
    prefix = "SSH_AUTH_SOCK="
    value = completed.stdout.strip()
    if completed.returncode != 0 or not value.startswith(prefix):
        return
    candidate = value[len(prefix) :]
    if not _agent_socket_answers(candidate):
        logger.line(
            f"Ignoring the SSH agent recorded in the tmux environment ({candidate}): the socket "
            "exists but nothing is listening on it. This is what a forwarded agent leaves behind "
            "when its session ends, and adopting it makes every later ssh hang instead of fail."
        )
        return
    os.environ["SSH_AUTH_SOCK"] = candidate
    logger.line("Using forwarded SSH agent from the tmux environment.")


PREFLIGHT_TIMEOUT_S = 45.0


def _unreachable_probe(detail: str) -> ProbeObservation:
    """The probe result for a source on a host preflight already proved unreachable.

    Synthesised rather than measured, deliberately: re-asking a host that just failed to answer
    `ssh <host> true` cannot learn anything the preflight did not, and would spend
    PROBE_TIMEOUT_S per source doing it. The detail carries the preflight's own message so the
    receipt says why, not merely that.
    """

    stamp = utc_now()
    return ProbeObservation(
        kind=ProbeKind.FAILED,
        exit_code=REMOTE_UNREACHABLE,
        estimated_bytes=None,
        resolved_source=None,
        detail=f"host did not answer preflight: {detail}",
        started_utc=stamp,
        finished_utc=stamp,
        duration_seconds=0.0,
    )


def preflight_hosts(
    operations: Sequence[Operation], logger: RunLogger
) -> dict[str, str]:
    """Prove each remote host answers BEFORE any per-source work begins.

    Returns host -> failure detail for the hosts that did not answer; an empty mapping means
    every host is reachable.

    The per-source probe already bounds itself, so this is not what stops a hang -- it is what
    stops a hang *per operation*. On the configured archive there are 24 operations across a
    handful of hosts, so one unauthenticable host costs 24 x PROBE_TIMEOUT_S of waiting to learn
    a fact that one four-second ssh could have established. Asking once per HOST rather than once
    per source is the whole saving, and it is why this is keyed on the host.

    ``ssh <host> true`` is the smallest question that exercises the thing that actually breaks:
    it resolves, connects, and AUTHENTICATES, which is the step `ConnectTimeout` does not cover.
    A host that fails here is recorded and skipped rather than aborting the run, because a fetch
    across several machines should still collect from the ones that are up -- the same reason
    this tool has never treated one bad source as fatal.
    """

    hosts: list[str] = []
    for operation in operations:
        if not operation.local and operation.machine.host not in hosts:
            hosts.append(operation.machine.host)
    if not hosts:
        return {}
    failures: dict[str, str] = {}
    for host in hosts:
        completed = _run_bounded(
            _ssh_command(host, "true"), timeout_s=PREFLIGHT_TIMEOUT_S
        )
        if completed.returncode == 0:
            continue
        detail = (completed.stderr or "").strip() or f"ssh exited {completed.returncode}"
        failures[host] = detail
        logger.line(f"Preflight: {host} did not answer: {detail}")
    if failures:
        logger.line(
            f"Preflight failed for {len(failures)} of {len(hosts)} host(s). Their sources are "
            "skipped; every reachable host is still fetched. The usual cause is no SSH agent or "
            "no Kerberos ticket -- check with `ssh-add -l`."
        )
    else:
        logger.line(f"Preflight: all {len(hosts)} remote host(s) answered.")
    return failures


def _base_result(
    operation: Operation,
    probe: ProbeObservation,
    status: str,
    exit_code: int,
    detail: str,
    *,
    finished_utc: str | None = None,
    duration_seconds: float | None = None,
    manifest: ManifestScan | None = None,
    destination_before_bytes: int = 0,
    destination_after_bytes: int = 0,
    metrics: RsyncMetrics | None = None,
    comparison: Comparison | None = None,
    warning_count: int = 0,
) -> OperationResult:
    effective_metrics = metrics if metrics is not None else RsyncMetrics()
    return OperationResult(
        machine=operation.machine.short_name,
        archive_name=operation.root.archive_name,
        requirement=operation.root.requirement.value,
        status=status,
        exit_code=exit_code,
        started_utc=probe.started_utc,
        finished_utc=finished_utc if finished_utc is not None else probe.finished_utc,
        duration_seconds=(
            duration_seconds if duration_seconds is not None else probe.duration_seconds
        ),
        source_estimated_bytes=probe.estimated_bytes,
        manifest_entries=len(manifest.entries) if manifest is not None else 0,
        manifest_regular_bytes=manifest.regular_bytes if manifest is not None else 0,
        destination_before_bytes=destination_before_bytes,
        destination_after_bytes=destination_after_bytes,
        rsync_total_files=effective_metrics.total_files,
        rsync_transferred_files=effective_metrics.transferred_files,
        rsync_total_file_size=effective_metrics.total_file_size,
        rsync_transferred_file_size=effective_metrics.transferred_file_size,
        rsync_literal_data=effective_metrics.literal_data,
        rsync_matched_data=effective_metrics.matched_data,
        rsync_bytes_sent=effective_metrics.bytes_sent,
        rsync_bytes_received=effective_metrics.bytes_received,
        retained_destination_entries=(
            comparison.retained_entries if comparison is not None else 0
        ),
        retained_destination_bytes=(
            comparison.retained_bytes if comparison is not None else 0
        ),
        stale_sqlite_sidecars=(
            comparison.stale_sqlite_sidecars if comparison is not None else 0
        ),
        retained_sqlite_databases=(
            comparison.retained_sqlite_databases if comparison is not None else 0
        ),
        rewrite_warnings=comparison.rewrite_warnings if comparison is not None else 0,
        warning_count=warning_count,
        detail=detail,
    )


def _result_for_probe(
    operation: Operation, probe: ProbeObservation, mode: Mode
) -> OperationResult:
    if probe.kind is ProbeKind.PRESENT:
        if mode is not Mode.CHECK_SOURCES:
            raise FetchError("internal error: present transfer probe finalized too early")
        return _base_result(operation, probe, "present", 0, probe.detail)
    if probe.kind is ProbeKind.MISSING:
        if operation.root.requirement is Requirement.OPTIONAL:
            return _base_result(
                operation,
                probe,
                "missing_optional",
                probe.exit_code,
                "optional source is absent",
            )
        return _base_result(
            operation,
            probe,
            "missing_required",
            probe.exit_code,
            "required source is absent",
        )
    if probe.kind is ProbeKind.NOT_DIRECTORY:
        status = "source_not_directory"
    elif probe.kind is ProbeKind.INACCESSIBLE:
        status = "source_inaccessible"
    elif probe.kind is ProbeKind.OUTSIDE_HOME:
        status = "source_outside_home"
    elif probe.kind is ProbeKind.ARCHIVE_OVERLAP:
        status = "source_overlaps_archive"
    else:
        status = "probe_failed"
    return _base_result(operation, probe, status, probe.exit_code, probe.detail)


def _warning_for_notice(
    operation: Operation, code: str, detail: str
) -> WarningRecord:
    return WarningRecord(
        timestamp_utc=utc_now(),
        machine=operation.machine.short_name,
        archive_name=operation.root.archive_name,
        code=code,
        path="",
        detail=detail,
    )


def _manifest_after_transfer(
    receipt: Receipt, operation: Operation, resolved_source: str
) -> ManifestScan:
    scan = scan_source_tree(operation, resolved_source)
    write_source_manifest(
        receipt.manifest_path(operation), operation, resolved_source, scan
    )
    return scan


def execute_transfer_operation(
    archive_dir: Path,
    state_dir: Path,
    operation: Operation,
    initial_probe: ProbeObservation,
    options: Options,
    receipt: Receipt,
    emit_warning: Callable[[WarningRecord], None],
    mark_rsync_started: Callable[[], None],
) -> OperationResult:
    began = time.monotonic()
    receipt.logger.line()
    receipt.logger.line(
        f"[{utc_now()}] {operation.machine.short_name} "
        f"{operation.root.archive_name} <- {operation.machine.host}:{operation.source}"
    )
    current_disk = disk_snapshot(archive_dir)
    headroom = max(current_disk.free - options.min_free_bytes, 0)
    estimate_exceeds_headroom = (
        options.min_free_bytes > 0
        and initial_probe.estimated_bytes is not None
        and initial_probe.estimated_bytes > headroom
    )
    if current_disk.free < options.min_free_bytes or estimate_exceeds_headroom:
        if current_disk.free < options.min_free_bytes:
            detail = (
                f"capacity preflight blocked transfer: {current_disk.free} free bytes is "
                f"below the {options.min_free_bytes} byte reserve"
            )
        else:
            detail = (
                f"capacity preflight blocked transfer: estimated source size "
                f"{initial_probe.estimated_bytes} exceeds {headroom} free bytes after "
                f"the {options.min_free_bytes} byte reserve"
            )
        emit_warning(_warning_for_notice(operation, "capacity_preflight_blocked", detail))
        return _base_result(
            operation,
            initial_probe,
            "capacity_blocked",
            1,
            detail,
            finished_utc=utc_now(),
            duration_seconds=time.monotonic() - began,
            warning_count=1,
        )

    destination_before = scan_destination_tree(operation.destination)
    immediate_probe = probe_source(
        operation, estimate_size=options.min_free_bytes > 0
    )
    if immediate_probe.kind is not ProbeKind.PRESENT:
        if immediate_probe.kind is ProbeKind.MISSING:
            status = "source_disappeared_before_transfer"
            detail = "source disappeared after the initial capacity probe"
        else:
            status = "probe_failed_before_transfer"
            detail = immediate_probe.detail
        return _base_result(
            operation,
            initial_probe,
            status,
            immediate_probe.exit_code,
            detail,
            finished_utc=utc_now(),
            duration_seconds=time.monotonic() - began,
            destination_before_bytes=destination_before.regular_bytes,
        )
    if immediate_probe.resolved_source is None:
        raise FetchError("present source probe did not provide a canonical source path")
    if (
        initial_probe.resolved_source is None
        or immediate_probe.resolved_source != initial_probe.resolved_source
    ):
        return _base_result(
            operation,
            initial_probe,
            "source_root_retargeted_before_transfer",
            1,
            "configured source resolved to a different path between preflight probes",
            finished_utc=utc_now(),
            duration_seconds=time.monotonic() - began,
            destination_before_bytes=destination_before.regular_bytes,
        )
    resolved_source = immediate_probe.resolved_source
    refreshed_disk = disk_snapshot(archive_dir)
    refreshed_headroom = max(refreshed_disk.free - options.min_free_bytes, 0)
    if (
        options.min_free_bytes > 0
        and immediate_probe.estimated_bytes is not None
        and immediate_probe.estimated_bytes > refreshed_headroom
    ):
        detail = (
            f"capacity preflight blocked transfer: refreshed source size "
            f"{immediate_probe.estimated_bytes} exceeds {refreshed_headroom} free bytes "
            f"after the {options.min_free_bytes} byte reserve"
        )
        emit_warning(_warning_for_notice(operation, "capacity_preflight_blocked", detail))
        return _base_result(
            operation,
            initial_probe,
            "capacity_blocked",
            1,
            detail,
            finished_utc=utc_now(),
            duration_seconds=time.monotonic() - began,
            destination_before_bytes=destination_before.regular_bytes,
            warning_count=1,
        )

    machine_dir = archive_dir / operation.machine.short_name
    ensure_safe_directory(archive_dir, machine_dir)
    ensure_safe_directory(archive_dir, operation.destination)
    partial_directory = (
        state_dir
        / "partials"
        / operation.machine.short_name
        / operation.root.archive_name
    )
    ensure_safe_directory(archive_dir, partial_directory)

    command = build_rsync_command(
        operation,
        resolved_source,
        operation.destination,
        partial_directory,
        options.mode,
    )
    mark_rsync_started()
    outcome = run_rsync(command, receipt.logger)
    source_after_probe = probe_source(operation, estimate_size=False)
    manifest: ManifestScan | None = None
    manifest_error = ""
    manifest_source_missing = False
    try:
        manifest = _manifest_after_transfer(receipt, operation, resolved_source)
    except ManifestSourceMissing as error:
        manifest_error = str(error)
        manifest_source_missing = True
    except ManifestError as error:
        manifest_error = str(error)
    source_retargeted = (
        source_after_probe.kind is ProbeKind.PRESENT
        and source_after_probe.resolved_source != resolved_source
    )

    destination_after = scan_destination_tree(operation.destination)
    validate_safe_directory_chain(
        archive_dir, operation.destination, leaf_may_be_missing=False
    )
    os.chmod(operation.destination, 0o700, follow_symlinks=False)

    warning_count = 0
    comparison: Comparison | None = None
    details: list[str] = []
    if manifest is not None:
        for notice in manifest.notices:
            warning = _warning_for_notice(operation, "source_manifest_unstable", notice)
            emit_warning(warning)
            warning_count += 1
        for notice in destination_before.notices:
            warning = _warning_for_notice(operation, "destination_scan_unstable", notice)
            emit_warning(warning)
            warning_count += 1
        for notice in destination_after.notices:
            warning = _warning_for_notice(operation, "destination_scan_unstable", notice)
            emit_warning(warning)
            warning_count += 1
        comparison = compare_source_and_destination(
            operation,
            manifest,
            destination_before,
            destination_after,
            actual_fetch=options.mode is Mode.FETCH and outcome.exit_code == 0,
        )
        for warning in comparison.warnings:
            emit_warning(warning)
            warning_count += 1
        details.extend(comparison.errors)
    if manifest_error:
        details.append(manifest_error)
    metrics_are_complete = rsync_metrics_complete(outcome.metrics)
    if not metrics_are_complete:
        details.append("rsync did not emit the complete expected statistics set")

    if outcome.exit_code == 0 and source_after_probe.kind is ProbeKind.MISSING:
        status = "source_disappeared_after_rsync"
        exit_code = 1
        details.append("configured source disappeared after rsync")
    elif outcome.exit_code == 0 and source_after_probe.kind is not ProbeKind.PRESENT:
        status = "post_rsync_probe_failed"
        exit_code = 1
        details.append(source_after_probe.detail)
    elif outcome.exit_code == 0 and source_retargeted:
        status = "source_root_retargeted"
        exit_code = 1
        details.append("configured source resolved to a different path after rsync")
    elif outcome.exit_code == 0 and manifest_source_missing:
        status = "source_disappeared_during_manifest"
        exit_code = 1
    elif outcome.exit_code == 0 and manifest_error:
        status = "manifest_failed"
        exit_code = 1
    elif outcome.exit_code == 0 and manifest is not None and manifest.notices:
        status = "manifest_unstable"
        exit_code = 1
    elif outcome.exit_code == 0 and comparison is not None and comparison.errors:
        status = "postcheck_failed"
        exit_code = 1
    elif outcome.exit_code == 0 and not metrics_are_complete:
        status = "metrics_incomplete"
        exit_code = 1
    elif outcome.exit_code == 0:
        status = "dry_run_complete" if options.mode is Mode.DRY_RUN else "fetched"
        exit_code = 0
    elif source_after_probe.kind is ProbeKind.MISSING:
        status = "source_disappeared_during_rsync"
        exit_code = outcome.exit_code
    elif source_after_probe.kind is not ProbeKind.PRESENT:
        status = "rsync_failed_source_unverifiable"
        exit_code = outcome.exit_code
    elif outcome.exit_code == 24:
        status = "source_changed_during_rsync"
        exit_code = outcome.exit_code
    else:
        status = "rsync_failed"
        exit_code = outcome.exit_code
    if not details:
        details.append(
            "rsync and post-transfer checks completed"
            if exit_code == 0
            else f"rsync exited {outcome.exit_code}"
        )
    result = _base_result(
        operation,
        initial_probe,
        status,
        exit_code,
        "; ".join(details),
        finished_utc=utc_now(),
        duration_seconds=time.monotonic() - began,
        manifest=manifest,
        destination_before_bytes=destination_before.regular_bytes,
        destination_after_bytes=destination_after.regular_bytes,
        metrics=outcome.metrics,
        comparison=comparison,
        warning_count=warning_count,
    )
    return result


def _require_commands(options: Options, operations: Sequence[Operation]) -> None:
    required: set[str] = set()
    if options.mode is not Mode.CHECK_SOURCES:
        required.update(("du", "rsync"))
    if any(not operation.local for operation in operations):
        required.add("ssh")
    missing = sorted(command for command in required if shutil.which(command) is None)
    if missing:
        raise FetchError("required command(s) not found: " + ", ".join(missing))


def _incomplete_result(
    operation: Operation, status: str, detail: str
) -> OperationResult:
    now = utc_now()
    probe = ProbeObservation(
        kind=ProbeKind.FAILED,
        exit_code=1,
        estimated_bytes=None,
        resolved_source=None,
        detail=detail,
        started_utc=now,
        finished_utc=now,
        duration_seconds=0.0,
    )
    return _base_result(operation, probe, status, 1, detail)


def _build_summary(
    run_id: str,
    started_utc: str,
    began: float,
    options: Options,
    operations: Sequence[Operation],
    results: Sequence[OperationResult],
    probed_operations: int,
    rsync_attempted_operations: int,
    warning_count: int,
    disk_before: DiskSnapshot,
    disk_after: DiskSnapshot,
) -> RunSummary:
    successful = sum(result.successful for result in results)
    failed = len(results) - successful
    complete_metrics = sum(
        rsync_metrics_complete(
            RsyncMetrics(
                total_files=result.rsync_total_files,
                transferred_files=result.rsync_transferred_files,
                total_file_size=result.rsync_total_file_size,
                transferred_file_size=result.rsync_transferred_file_size,
                literal_data=result.rsync_literal_data,
                matched_data=result.rsync_matched_data,
                bytes_sent=result.rsync_bytes_sent,
                bytes_received=result.rsync_bytes_received,
            )
        )
        for result in results
    )
    status = "failed" if failed else ("complete_with_warnings" if warning_count else "complete")
    exit_code = 1 if failed else 0
    return RunSummary(
        run_id=run_id,
        started_utc=started_utc,
        finished_utc=utc_now(),
        duration_seconds=time.monotonic() - began,
        mode=options.mode.value,
        status=status,
        exit_code=exit_code,
        expected_operations=len(operations),
        recorded_operations=len(results),
        successful_operations=successful,
        failed_operations=failed,
        optional_missing=sum(result.status == "missing_optional" for result in results),
        probed_operations=probed_operations,
        rsync_attempted_operations=rsync_attempted_operations,
        rsync_metrics_complete_operations=complete_metrics,
        rsync_metrics_unknown_operations=max(
            rsync_attempted_operations - complete_metrics, 0
        ),
        manifest_entries=sum(result.manifest_entries for result in results),
        manifest_regular_bytes=sum(result.manifest_regular_bytes for result in results),
        rsync_transferred_files=sum(result.rsync_transferred_files or 0 for result in results),
        rsync_transferred_file_size=sum(
            result.rsync_transferred_file_size or 0 for result in results
        ),
        rsync_literal_data=sum(result.rsync_literal_data or 0 for result in results),
        rsync_matched_data=sum(result.rsync_matched_data or 0 for result in results),
        rsync_bytes_sent=sum(result.rsync_bytes_sent or 0 for result in results),
        rsync_bytes_received=sum(result.rsync_bytes_received or 0 for result in results),
        retained_destination_entries=sum(
            result.retained_destination_entries for result in results
        ),
        retained_destination_bytes=sum(
            result.retained_destination_bytes for result in results
        ),
        stale_sqlite_sidecars=sum(result.stale_sqlite_sidecars for result in results),
        retained_sqlite_databases=sum(
            result.retained_sqlite_databases for result in results
        ),
        rewrite_warnings=sum(result.rewrite_warnings for result in results),
        warning_count=warning_count,
        disk_total_bytes=disk_before.total,
        disk_free_before_bytes=disk_before.free,
        disk_free_after_bytes=disk_after.free,
    )


def execute_run(
    archive_dir: Path,
    configuration_snapshot: ConfigurationSnapshot,
    fetcher_file: Path,
    options: Options,
    operations: Sequence[Operation],
) -> int:
    validate_archive_directory(archive_dir)
    validate_operation_destinations(archive_dir, operations)
    _require_commands(options, operations)
    runs_dir = archive_dir / "_fetch_runs"
    state_dir = archive_dir / "_fetch_state"
    ensure_safe_directory(archive_dir, runs_dir)
    ensure_safe_directory(archive_dir, state_dir)
    lock_path = state_dir / "fetch.lock"
    with FetchLock(lock_path):
        backfill_history(archive_dir, runs_dir)
        run_id = make_run_id()
        started_utc = utc_now()
        began = time.monotonic()
        disk_before = disk_snapshot(archive_dir)
        receipt = Receipt(
            archive_dir=archive_dir,
            run_id=run_id,
            options=options,
            expected_operations=len(operations),
            configuration_snapshot=configuration_snapshot,
            fetcher_file=fetcher_file,
            started_utc=started_utc,
        )
        results: list[OperationResult] = []
        recorded_keys: set[str] = set()
        probed_operations = 0
        rsync_attempted_operations = 0
        warning_count = 0

        def emit_warning(warning: WarningRecord) -> None:
            nonlocal warning_count
            warning_count += 1
            receipt.record_warning(warning)
            location = f" {warning.path}" if warning.path else ""
            receipt.logger.line(
                f"WARNING [{warning.code}]{location}: {warning.detail}", error=True
            )

        def record(result: OperationResult) -> None:
            key = result.machine + "/" + result.archive_name
            if key in recorded_keys:
                raise FetchError(f"internal error: duplicate operation result for {key}")
            recorded_keys.add(key)
            results.append(result)
            receipt.record_result(result)
            receipt.logger.line(
                f"Result: {key}: {result.status} (exit {result.exit_code})"
            )

        def mark_rsync_started() -> None:
            nonlocal rsync_attempted_operations
            rsync_attempted_operations += 1

        receipt.logger.line(f"Run: {run_id}")
        receipt.logger.line(f"Mode: {options.mode.value}")
        receipt.logger.line(f"Started: {started_utc}")
        receipt.logger.line(f"Archive: {archive_dir}")
        receipt.logger.line(f"Selected operations: {len(operations)}")
        unfinished_status = "not_attempted_internal_error"
        unfinished_detail = "run stopped before this operation completed"
        try:
            discover_forwarded_ssh_agent(receipt.logger)
            unreachable_hosts = preflight_hosts(operations, receipt.logger)
            probes: dict[str, ProbeObservation] = {}
            estimate_size = options.mode is not Mode.CHECK_SOURCES
            for operation in operations:
                probe = (
                    _unreachable_probe(
                        unreachable_hosts[operation.machine.host]
                    )
                    if not operation.local
                    and operation.machine.host in unreachable_hosts
                    else probe_source(operation, estimate_size=estimate_size)
                )
                if (
                    operation.local
                    and probe.kind is ProbeKind.PRESENT
                    and probe.resolved_source is not None
                ):
                    resolved = Path(probe.resolved_source)
                    if _is_within(archive_dir, resolved) or _is_within(
                        resolved, archive_dir
                    ):
                        probe = replace(
                            probe,
                            kind=ProbeKind.ARCHIVE_OVERLAP,
                            exit_code=1,
                            detail=(
                                "local source overlaps the archive directory and could "
                                "recursively copy live archive state"
                            ),
                        )
                probes[operation.key] = probe
                probed_operations += 1
                receipt.logger.line(
                    f"Probe {operation.key}: {probe.kind.value}"
                    + (
                        f" ({probe.estimated_bytes} estimated bytes)"
                        if probe.estimated_bytes is not None
                        else ""
                    )
                )

            estimated_source_bytes = sum(
                probe.estimated_bytes or 0 for probe in probes.values()
            )
            headroom = max(disk_before.free - options.min_free_bytes, 0)
            if options.mode is not Mode.CHECK_SOURCES and estimated_source_bytes > headroom:
                emit_warning(
                    WarningRecord(
                        timestamp_utc=utc_now(),
                        machine="",
                        archive_name="",
                        code="capacity_estimate_exceeds_headroom",
                        path="",
                        detail=(
                            f"selected sources contain approximately {estimated_source_bytes} "
                            f"bytes, above {headroom} free bytes after reserve; this is a "
                            "conservative warning because rsync transfers only differences"
                        ),
                    )
                )
            free_percent = 100.0 * disk_before.free / disk_before.total
            if free_percent < LOW_FREE_PERCENT:
                emit_warning(
                    WarningRecord(
                        timestamp_utc=utc_now(),
                        machine="",
                        archive_name="",
                        code="low_disk_free_percent",
                        path="",
                        detail=f"archive filesystem is only {free_percent:.2f}% free",
                    )
                )

            for operation in operations:
                probe = probes[operation.key]
                if probe.kind is not ProbeKind.PRESENT or options.mode is Mode.CHECK_SOURCES:
                    record(_result_for_probe(operation, probe, options.mode))
                    continue
                try:
                    result = execute_transfer_operation(
                        archive_dir,
                        state_dir,
                        operation,
                        probe,
                        options,
                        receipt,
                        emit_warning,
                        mark_rsync_started,
                    )
                    record(result)
                except RunInterrupted:
                    raise
                except Exception as error:
                    record(
                        _base_result(
                            operation,
                            probe,
                            "internal_error",
                            1,
                            str(error),
                            finished_utc=utc_now(),
                        )
                    )
                    receipt.logger.line(
                        f"Operation {operation.key} raised: {error}", error=True
                    )
        except KeyboardInterrupt:
            unfinished_status = "interrupted"
            unfinished_detail = "run interrupted before this operation completed"
            receipt.logger.line("Run interrupted by the user.", error=True)
        except RunInterrupted as error:
            unfinished_status = "interrupted"
            unfinished_detail = str(error)
            receipt.logger.line(str(error), error=True)
        except Exception as error:
            unfinished_detail = f"run stopped after internal error: {error}"
            receipt.logger.line(f"Run failed unexpectedly: {error}", error=True)
        finally:
            previous_sigterm = signal.getsignal(signal.SIGTERM)
            previous_sigint = signal.getsignal(signal.SIGINT)
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            try:
                for operation in operations:
                    if operation.key not in recorded_keys:
                        record(
                            _incomplete_result(
                                operation,
                                unfinished_status,
                                unfinished_detail,
                            )
                        )
                disk_after = disk_snapshot(archive_dir)
                summary = _build_summary(
                    run_id,
                    started_utc,
                    began,
                    options,
                    operations,
                    results,
                    probed_operations,
                    rsync_attempted_operations,
                    warning_count,
                    disk_before,
                    disk_after,
                )
                receipt.finalize(summary)
            finally:
                signal.signal(signal.SIGINT, previous_sigint)
                signal.signal(signal.SIGTERM, previous_sigterm)
        return summary.exit_code


def _main(arguments: Sequence[str]) -> int:
    try:
        options = parse_options(arguments)
        if options is None:
            return 0
        fetcher_file = Path(__file__).resolve()
        archive_dir = fetcher_file.parent
        if options.history_only:
            validate_archive_directory(archive_dir)
            if options.backfill_history:
                runs_dir = archive_dir / "_fetch_runs"
                state_dir = archive_dir / "_fetch_state"
                ensure_safe_directory(archive_dir, runs_dir)
                ensure_safe_directory(archive_dir, state_dir)
                with FetchLock(state_dir / "fetch.lock"):
                    additions, migrated = backfill_history(archive_dir, runs_dir)
                print(
                    f"history: added {additions} missing run(s); "
                    f"legacy migration={'yes' if migrated else 'no'}",
                    file=sys.stderr,
                )
            print_history(archive_dir)
            return 0
        machines_file = archive_dir / "machines.tsv"
        roots_file = archive_dir / "log_roots.tsv"
        configuration_snapshot = load_configuration(machines_file, roots_file)
        configuration = configuration_snapshot.configuration
        operations = build_operations(configuration, options, archive_dir)
        if options.list_only:
            print_configuration(configuration, operations)
            return 0
        return execute_run(
            archive_dir,
            configuration_snapshot,
            fetcher_file,
            options,
            operations,
        )
    except FetchError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


def main(argv: Sequence[str] | None = None) -> int:
    os.umask(0o077)
    arguments = tuple(argv) if argv is not None else tuple(sys.argv[1:])
    previous_sigterm = signal.getsignal(signal.SIGTERM)
    signal.signal(signal.SIGTERM, _raise_run_interrupted)
    try:
        return _main(arguments)
    finally:
        signal.signal(signal.SIGTERM, previous_sigterm)


if __name__ == "__main__":
    raise SystemExit(main())

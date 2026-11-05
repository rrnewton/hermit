"""Durable step output and conservative test-boundary attribution.

This is the Python peer of the native runner's attribution stream. Evidence is explicit opt-in via
``DAGRUN_LOG_DIR``; paths are opened nonblocking/no-follow and accepted only as private,
owned regular files so a stale FIFO or symlink cannot hang or redirect a run.
"""

from __future__ import annotations

import json
import os
import stat
import sys
import threading
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import BinaryIO, Literal

from dagrun.procstat import parse_process_stat

LOG_DIR_ENV = "DAGRUN_LOG_DIR"
NO_LOGS_ENV = "DAGRUN_NO_STEP_LOGS"
LOG_MAX_BYTES_ENV = "DAGRUN_LOG_MAX_BYTES"

# Default per-step durable-log ceiling: 1 GiB.
#
# WHY A CEILING AT ALL. A step log is a byte-for-byte copy of the step's raw output --
# measured, not estimated: a step emitting exactly 100 MiB produced a 104,857,600-byte log.
# So a step that runs away on stdout does not merely go unbounded, it is DUPLICATED onto the
# filesystem. On 2026-08-17..19 three invocations in a consuming repository wrote ~4.5 TB of
# stderr each and filled the device; had evidence capture been enabled for those runs, this
# log would have written a second copy of every byte onto the same device.
#
# WHY 1 GiB. Large enough that no honest step is truncated, small enough that a runaway is
# stopped while the filesystem still has room to be useful.
DEFAULT_LOG_MAX_BYTES = 1024 * 1024 * 1024

# Exact bytes appended once when a step log hits its ceiling.
#
# PARITY: the Rust and Python engines are compared byte-for-byte by the differential harness,
# so this marker must stay identical in both. Keep the two in sync or `make cross` fails.
TRUNCATION_MARKER = (
    "\n[dagrun] STEP LOG TRUNCATED at this ceiling "
    "(raise or lift it with DAGRUN_LOG_MAX_BYTES; 0 = unlimited). "
    "Test classification and attribution CONTINUE; only durable capture stopped.\n"
)


def require_step_end_ok(record: Mapping[str, object]) -> bool:
    """Return a ``step_end`` verdict, refusing absent or non-boolean values."""
    if record.get("event") != "step_end":
        raise ValueError("journal record is not a step_end event")
    value = record.get("ok")
    if type(value) is not bool:
        raise ValueError("journal step_end ok must be a boolean")
    return value


CAPTURE_MAX_BYTES_ENV = "DAGRUN_CAPTURE_MAX_BYTES"

# Default per-step IN-MEMORY capture ceiling: 4 MiB of the step's output TAIL.
#
# WHY A SECOND CEILING. :data:`DEFAULT_LOG_MAX_BYTES` bounds the copy on DISK. It says nothing
# about the copy the runner holds in its own address space for the whole life of the step, and
# that one is the copy that kills the runner first: a step whose output runs away is held
# entirely in RSS, and the failure path then joins it into one contiguous buffer, doubling the
# peak at exactly the worst moment. A multi-terabyte runaway OOMs the runner long before it
# fills the device the disk ceiling is protecting.
#
# WHY A TAIL, AND WHY THIS SIZE. Everything the in-memory capture feeds is a tail: the one-line
# summary, and the failure dump a human actually reads. 4 MiB is far more of a tail than any
# human reads and still bounds the whole-step peak at 2x that plus a constant. The DURABLE log
# is unaffected and remains the place to go for the complete output.
DEFAULT_CAPTURE_MAX_BYTES = 4 * 1024 * 1024

# Exact notice prepended to a failure dump whose in-memory capture dropped earlier output.
#
# PARITY: the Rust and Python engines are compared byte-for-byte by the differential harness,
# so this must stay identical in both. It takes (total_bytes, kept_bytes).
CAPTURE_TRUNCATION_NOTICE = (
    "[dagrun] EARLIER OUTPUT DROPPED: this step produced {total} bytes but only "
    "the last {kept} were kept in memory (raise or lift the ceiling with "
    "DAGRUN_CAPTURE_MAX_BYTES; 0 = unlimited). The durable per-step log is "
    "unaffected and still has the rest."
)


def capture_max_bytes() -> int | None:
    """The per-step in-memory capture ceiling in bytes, or ``None`` for unlimited.

    Same posture as :func:`log_max_bytes`: an unparseable value is REPORTED and then treated as
    the default, because a misread ceiling that quietly becomes "unlimited" is precisely the
    failure the ceiling exists to prevent.
    """
    raw = os.environ.get(CAPTURE_MAX_BYTES_ENV)
    if raw is None or not raw.strip():
        return DEFAULT_CAPTURE_MAX_BYTES
    try:
        value = int(raw.strip())
        if value < 0:
            raise ValueError(raw)
    except ValueError:
        print(
            f"[dagrun] WARNING: {CAPTURE_MAX_BYTES_ENV}={raw!r} is not a "
            f"non-negative integer; using the {DEFAULT_CAPTURE_MAX_BYTES}-byte default.",
            file=sys.stderr,
        )
        return DEFAULT_CAPTURE_MAX_BYTES
    return None if value == 0 else value


def log_max_bytes() -> int | None:
    """Return the per-step durable-log ceiling in bytes, or ``None`` for unlimited.

    An unparseable value is reported and then treated as the default rather than silently
    ignored -- a misread ceiling that quietly becomes "unlimited" is the failure this
    ceiling exists to prevent.
    """
    raw = os.environ.get(LOG_MAX_BYTES_ENV)
    if raw is None or not raw.strip():
        return DEFAULT_LOG_MAX_BYTES
    try:
        value = int(raw.strip())
        if value < 0:
            raise ValueError(raw)
    except ValueError:
        print(
            f"[dagrun] WARNING: {LOG_MAX_BYTES_ENV}={raw!r} is not a "
            f"non-negative integer; using the {DEFAULT_LOG_MAX_BYTES}-byte default.",
            file=sys.stderr,
        )
        return DEFAULT_LOG_MAX_BYTES
    return None if value == 0 else value


_MAX_COMPONENT_BYTES = 255


def sanitize(tag: str) -> str:
    """Injectively encode a UTF-8 tag as an ASCII filename component."""
    safe = bytearray()
    for byte in tag.encode("utf-8"):
        if (
            ord("A") <= byte <= ord("Z")
            or ord("a") <= byte <= ord("z")
            or ord("0") <= byte <= ord("9")
            or byte in b"._-"
        ):
            safe.append(byte)
        else:
            safe.extend(f"~{byte:02x}".encode("ascii"))
    return safe.decode("ascii")


def default_log_dir() -> Path | None:
    """Return the explicitly configured evidence directory, if any."""
    if os.environ.get(NO_LOGS_ENV) == "1":
        return None
    value = os.environ.get(LOG_DIR_ENV, "")
    return Path(value) if value else None


def _private_owned_regular(fd: int) -> bool:
    try:
        metadata = os.fstat(fd)
    except OSError:
        return False
    return (
        stat.S_ISREG(metadata.st_mode)
        and metadata.st_uid == os.geteuid()
        and metadata.st_nlink == 1
        and metadata.st_mode & 0o077 == 0
    )


def _open_private_regular(path: Path, flags: int) -> BinaryIO | None:
    try:
        fd = os.open(
            path,
            flags | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
            0o600,
        )
    except OSError:
        return None
    try:
        if not _private_owned_regular(fd):
            os.close(fd)
            return None
        return os.fdopen(fd, "ab", buffering=0)
    except BaseException:
        os.close(fd)
        raise


class RunEvidence:
    """One private journal plus per-step raw output logs."""

    def __init__(self, directory: Path, journal: BinaryIO):
        self.directory = directory
        self._journal = journal
        self._lock = threading.Lock()

    @classmethod
    def open(cls, directory: Path | None) -> "RunEvidence | None":
        """Open private run evidence at ``directory``, or disable it safely.

        Existing paths are accepted only when they are owned by this user and inaccessible to
        group/other users.  Special files, symlinks, and hard links are rejected.
        """
        if directory is None:
            return None
        try:
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            metadata = directory.lstat()
        except OSError as exc:
            print(
                f"[scheduler] WARNING: could not create/inspect evidence directory "
                f"{directory} ({exc}); per-step logs and attribution are disabled for this run.",
                file=sys.stderr,
            )
            return None
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or metadata.st_mode & 0o700 != 0o700
            or metadata.st_mode & 0o077 != 0
        ):
            print(
                f"[scheduler] WARNING: evidence directory {directory} is not a private, owned "
                "directory; per-step logs and attribution are disabled for this run.",
                file=sys.stderr,
            )
            return None
        journal_path = directory / "journal.jsonl"
        journal = _open_private_regular(journal_path, os.O_WRONLY | os.O_APPEND)
        if journal is None:
            print(
                f"[scheduler] WARNING: evidence journal {journal_path} is not a private, owned "
                "regular file; per-step logs and attribution are disabled for this run.",
                file=sys.stderr,
            )
            return None
        return cls(directory, journal)

    def record(self, event: str, fields: Sequence[tuple[str, object]]) -> None:
        """Append and flush one structured journal record."""
        millis = time.time_ns() // 1_000_000
        payload: dict[str, object] = {
            "ts": f"{millis // 1000}.{millis % 1000:03}",
            "event": event,
        }
        payload.update(fields)
        line = (json.dumps(payload, separators=(",", ":"), ensure_ascii=False) + "\n").encode()
        with self._lock:
            try:
                self._journal.write(line)
                self._journal.flush()
            except OSError:
                pass

    def open_step_log(self, tag: str) -> BinaryIO | None:
        """Create or truncate the private raw-output log for ``tag``."""
        name = f"{sanitize(tag)}.log"
        if len(name.encode("ascii")) > _MAX_COMPONENT_BYTES:
            return None
        handle = _open_private_regular(self.directory / name, os.O_WRONLY)
        if handle is None:
            return None
        try:
            os.ftruncate(handle.fileno(), 0)
        except OSError:
            handle.close()
            return None
        return handle


@dataclass(frozen=True)
class TestEvent:
    """A conservatively recognized test start or completion boundary."""

    kind: Literal["start", "end"]
    name: str
    verdict: str = ""


def recognize(line: str) -> TestEvent | None:
    """Conservatively recognize explicit, libtest, pytest, or TAP boundaries."""
    raw = line.lstrip().rstrip("\n\r")
    stripped = raw.rstrip()
    if stripped.startswith("##TEST-START "):
        name = stripped.removeprefix("##TEST-START ").strip()
        return TestEvent("start", name) if name else None
    if stripped.startswith("##TEST-END "):
        parts = stripped.removeprefix("##TEST-END ").strip().split(maxsplit=1)
        return TestEvent("end", parts[0], parts[1] if len(parts) > 1 else "end") if parts else None
    if raw.startswith("test "):
        rest = raw.removeprefix("test ")
        if " ... " in rest:
            name, after = rest.split(" ... ", 1)
        elif rest.endswith(" ..."):
            name, after = rest.removesuffix(" ..."), ""
        else:
            name, after = "", ""
        name = name.strip()
        if name and " " not in name:
            verdict = after.strip()
            return TestEvent("end", name, verdict.split()[0]) if verdict else TestEvent("start", name)
    parts = stripped.rsplit(maxsplit=1)
    if len(parts) == 2:
        head, verdict = parts
        if (
            "::" in head
            and " " not in head
            and verdict in {"PASSED", "FAILED", "ERROR", "SKIPPED", "XFAIL", "XPASS"}
        ):
            return TestEvent("end", head, verdict)
    if "::" in stripped and " " not in stripped and len(stripped) > 2:
        return TestEvent("start", stripped)
    for prefix, verdict in (("not ok ", "not ok"), ("ok ", "ok")):
        if stripped.startswith(prefix):
            rest = stripped.removeprefix(prefix)
            if " - " in rest:
                number, name = rest.split(" - ", 1)
                if number.strip().isdigit() and name.strip():
                    return TestEvent("end", name.strip(), verdict)
    return None


@dataclass
class TestTracker:
    """Incremental counts and most-recent test identities for one step."""

    last_started: str | None = None
    last_completed: str | None = None
    completed: int = 0
    started: int = 0
    in_flight: dict[str, float] = field(default_factory=dict)

    def observe(self, event: TestEvent) -> None:
        """Apply one recognized boundary to this tracker."""
        if event.kind == "start":
            self.last_started = event.name
            if event.name not in self.in_flight:
                self.started += 1
                self.in_flight[event.name] = time.monotonic()
        else:
            if self.last_started != event.name:
                self.last_started = event.name
                self.started += 1
            self.in_flight.pop(event.name, None)
            self.last_completed = event.name
            self.completed += 1

    def in_flight_snapshot(self) -> tuple["InFlightTest", ...]:
        """Return every currently declared test, longest-running first."""
        now = time.monotonic()
        return tuple(
            sorted(
                (
                    InFlightTest(name, max(0.0, now - started), "declared test boundary")
                    for name, started in self.in_flight.items()
                ),
                key=lambda test: (-test.elapsed_s, test.name),
            )
        )


@dataclass(frozen=True)
class InFlightTest:
    """One test known to be live at the termination boundary."""

    name: str
    elapsed_s: float
    basis: str


@dataclass(frozen=True)
class Culprit:
    """Best-effort attribution for the test active when a step stopped."""

    test: str | None
    how: str
    completed: int
    last_completed: str | None
    in_flight: tuple[InFlightTest, ...] = ()

    def describe(self) -> str:
        """Render a concise human-readable attribution summary."""
        last = f", last completed {self.last_completed}" if self.last_completed else ""
        live = (
            f"; {len(self.in_flight)} test(s) in flight ["
            + ", ".join(
                f"{test.name} {test.elapsed_s:.3f}s via {test.basis}"
                for test in self.in_flight
            )
            + "]"
            if self.in_flight
            else ""
        )
        if self.test is not None:
            return (
                f"{'likely ' if len(self.in_flight) > 1 else ''}culprit test {self.test} "
                f"({self.how}; {self.completed} test(s) completed first{last}{live})"
            )
        return (
            f"no test-level attribution ({self.how}; {self.completed} test(s) "
            f"completed{last}{live})"
        )


@dataclass(frozen=True)
class ProcessObservation:
    """One owned process observed immediately before graceful termination."""

    pid: int
    ppid: int
    command: str
    wall_elapsed_s: float
    cpu_elapsed_s: float
    signature: str
    test: str | None = None
    test_basis: str | None = None


def _exact_libtest_from_argv(argv: list[str]) -> str | None:
    try:
        exact = argv.index("--exact")
    except ValueError:
        return None
    # libtest accepts both `--exact TEST` and `TEST --exact`; nextest currently uses the former.
    candidates = argv[exact + 1 : exact + 2] + argv[max(1, exact - 1) : exact]
    return next(
        (candidate for candidate in candidates if candidate and not candidate.startswith("-")),
        None,
    )


def _process_signature(state: str, wall_elapsed_s: float, cpu_elapsed_s: float) -> str:
    ratio = cpu_elapsed_s / wall_elapsed_s if wall_elapsed_s > 0 else 0.0
    # A runnable process is direct evidence of current CPU demand. Preserve that evidence under
    # host contention, where an actual busy loop can receive less than half of elapsed wall time.
    if cpu_elapsed_s >= 0.25 and (ratio >= 0.50 or state == "R"):
        return "cpu-burning"
    if wall_elapsed_s >= 0.50 and ratio <= 0.05:
        return "wall-stalled"
    return "mixed-or-too-young"


def process_snapshot(root: int, nonce: str | None) -> tuple[ProcessObservation, ...]:
    """Snapshot root descendants plus exact-nonce escapees without guessing by process name."""
    if root <= 1:
        return ()
    rows: dict[int, tuple[int, str, int, int, int, list[str], bool]] = {}
    needle = f"DAGRUN_STEP={nonce}".encode() if nonce else None
    try:
        entries = list(Path("/proc").iterdir())
    except OSError:
        return ()
    for entry in entries:
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            stat = parse_process_stat((entry / "stat").read_bytes())
            if stat is None or stat.pid != pid:
                continue
            state = stat.state
            ppid = stat.ppid
            utime = stat.utime
            stime = stat.stime
            start = stat.starttime
            raw_cmd = (entry / "cmdline").read_bytes()
            argv = [part.decode(errors="replace") for part in raw_cmd.split(b"\0") if part]
            carries = False
            if needle:
                try:
                    carries = needle in (entry / "environ").read_bytes().split(b"\0")
                except OSError:
                    pass
        except OSError:
            continue
        rows[pid] = (ppid, state, utime, stime, start, argv, carries)
    owned = {root}
    while True:
        before = len(owned)
        for pid, (ppid, _state, _utime, _stime, _start, _argv, carries) in rows.items():
            if ppid in owned or carries:
                owned.add(pid)
        if len(owned) == before:
            break
    try:
        ticks = float(os.sysconf("SC_CLK_TCK"))
        boot_s = time.clock_gettime(time.CLOCK_MONOTONIC)
    except (OSError, ValueError):
        return ()
    result: list[ProcessObservation] = []
    for pid, (ppid, state, utime, stime, start, argv, _carries) in rows.items():
        if pid not in owned or state == "Z":
            continue
        wall = max(0.0, boot_s - start / ticks)
        cpu = (utime + stime) / ticks
        signature = _process_signature(state, wall, cpu)
        test = _exact_libtest_from_argv(argv)
        command = " ".join(argv) if argv else f"[pid {pid}]"
        if len(command) > 512:
            command = command[:512] + "..."
        result.append(
            ProcessObservation(
                pid,
                ppid,
                command,
                wall,
                cpu,
                signature,
                test,
                "libtest --exact process argv" if test else None,
            )
        )
    return tuple(sorted(result, key=lambda row: row.pid))


def bind_process_tests(
    culprit: Culprit, observations: tuple[ProcessObservation, ...]
) -> Culprit:
    """Use exact nextest/libtest process bindings only when output had no stronger answer."""
    if culprit.test is not None or culprit.in_flight:
        return culprit
    by_name: dict[str, InFlightTest] = {}
    for row in observations:
        if row.test is None:
            continue
        current = by_name.get(row.test)
        candidate = InFlightTest(
            row.test, row.wall_elapsed_s, row.test_basis or "process observation"
        )
        if current is None or candidate.elapsed_s > current.elapsed_s:
            by_name[row.test] = candidate
    tests = tuple(sorted(by_name.values(), key=lambda test: (-test.elapsed_s, test.name)))
    if not tests:
        return culprit
    return Culprit(
        tests[0].name,
        "only test-bound process live at termination"
        if len(tests) == 1
        else "longest-running test-bound process at termination",
        culprit.completed,
        culprit.last_completed,
        tests,
    )


class StepStream:
    """One step's raw log, unterminated output tail, and test tracker."""

    def __init__(self, tag: str, evidence: RunEvidence | None):
        self.tag = tag
        self.evidence = evidence
        self._log = evidence.open_step_log(tag) if evidence is not None else None
        self._partial = ""
        self._tracker = TestTracker()
        self._tail_announced: str | None = None
        self._lock = threading.Lock()
        self._log_max_bytes = log_max_bytes()
        self._written = 0
        self._truncation_announced = False

    def ingest(self, data: bytes) -> None:
        """Persist and classify one raw output chunk."""
        truncated_now = False
        with self._lock:
            # DURABLE CAPTURE IS BOUNDED; CLASSIFICATION IS NOT. Everything below this block --
            # line splitting, test recognition, the unterminated-tail marker -- runs on every
            # byte regardless of the ceiling. A step that floods stdout still gets its hung test
            # named; it just stops being copied to disk. Bounding the disk must not cost the
            # attribution the disk was there to support.
            if self._log is not None:
                limit = self._log_max_bytes
                try:
                    if limit is None:
                        self._log.write(data)
                        self._log.flush()
                        self._written += len(data)
                    elif self._written < limit:
                        # Write only the prefix that fits, so the ceiling is exact rather than
                        # "the last chunk that crossed it", which would make the bound depend on
                        # the reader's buffer size and differ between the two engines.
                        take = min(limit - self._written, len(data))
                        self._log.write(data[:take])
                        self._written += take
                        if self._written >= limit and not self._truncation_announced:
                            self._log.write(TRUNCATION_MARKER.encode())
                            self._truncation_announced = True
                            truncated_now = True
                        self._log.flush()
                    # At or past the ceiling and already announced: drop the bytes.
                except OSError:
                    pass
            # Journal the truncation BEFORE this chunk's classification records, matching the
            # Rust engine's ordering so journal.jsonl compares equal under `make cross`. A
            # capped log that does not say it is capped is a silently incomplete evidence
            # file, which is worse than no evidence file: a later reader cannot tell "the step
            # printed nothing more" from "we stopped writing".
            if truncated_now and self.evidence is not None:
                self.evidence.record(
                    "step_log_truncated",
                    [("step", self.tag), ("limit_bytes", str(self._log_max_bytes))],
                )
            self._partial += data.decode(errors="replace")
            while "\n" in self._partial:
                line, self._partial = self._partial.split("\n", 1)
                self._classify(line.rstrip("\r"))
            if len(self._partial) > 8192:
                self._partial = self._partial[-4096:]
            event = recognize(self._partial)
            if event is not None and event.kind == "start" and self._tail_announced != event.name:
                self._tail_announced = event.name
                self._tracker.observe(event)
                if self.evidence is not None:
                    self.evidence.record("test_start", [("step", self.tag), ("test", event.name)])

    def _classify(self, line: str) -> None:
        event = recognize(line)
        if event is None:
            return
        if self._tail_announced == event.name:
            self._tail_announced = None
            if event.kind == "end":
                self._tracker.in_flight.pop(event.name, None)
                self._tracker.last_completed = event.name
                self._tracker.completed += 1
                if self.evidence is not None:
                    self.evidence.record(
                        "test_end",
                        [("step", self.tag), ("test", event.name), ("verdict", event.verdict)],
                    )
            return
        self._tracker.observe(event)
        if self.evidence is not None:
            self.evidence.record(
                "test_start" if event.kind == "start" else "test_end",
                [("step", self.tag), ("test", event.name)]
                + ([] if event.kind == "start" else [("verdict", event.verdict)]),
            )

    def culprit(self) -> Culprit:
        """Return the strongest conservative culprit supported by output so far."""
        with self._lock:
            tail = self._partial.lstrip().rstrip("\n\r")
            event = recognize(tail) if tail.strip() else None
            tracker = TestTracker(
                last_started=self._tracker.last_started,
                last_completed=self._tracker.last_completed,
                completed=self._tracker.completed,
                started=self._tracker.started,
                in_flight=dict(self._tracker.in_flight),
            )
        in_flight = tracker.in_flight_snapshot()
        if in_flight:
            return Culprit(
                in_flight[0].name,
                "declared in flight and never completed"
                if len(in_flight) == 1
                else "longest-running of the declared in-flight tests",
                tracker.completed,
                tracker.last_completed,
                in_flight,
            )
        if event is not None and event.kind == "start":
            return Culprit(
                event.name,
                "announced in the unterminated output tail, never completed",
                tracker.completed,
                tracker.last_completed,
            )
        if tracker.last_started is not None and tracker.last_completed != tracker.last_started:
            return Culprit(
                tracker.last_started,
                "started and never completed",
                tracker.completed,
                tracker.last_completed,
            )
        how = (
            "no test boundaries were recognized in this step's output"
            if tracker.completed == 0
            else "every recognized test completed; the step hung after the last one"
        )
        return Culprit(None, how, tracker.completed, tracker.last_completed)

    def counts(self) -> TestTracker:
        """Return an immutable-in-practice snapshot of current tracker fields."""
        with self._lock:
            return TestTracker(
                last_started=self._tracker.last_started,
                last_completed=self._tracker.last_completed,
                completed=self._tracker.completed,
                started=self._tracker.started,
                in_flight=dict(self._tracker.in_flight),
            )

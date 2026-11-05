"""Opt-in profiler captures at a step's model-selected parallelism width.

Scaling sweeps answer *where* a step should run.  This module performs the deliberately more
expensive follow-up experiment at that width: one ``perf record`` trial and/or one or more short,
centred ``wprof`` trials.  Instrumented trials are separate executions and are never ordinary
profile samples; feeding their observer overhead back into the scaling model would corrupt the
model that selected them.

The command-line layer supplies :func:`capture_at_sweet_spot` with a callback that runs exactly
one isolated copy of the step.  For a perf trial the callback prepends ``request.argv_prefix`` to
the command it launches.  Wprof is a system-wide sidecar and therefore receives an empty prefix.
The actual guest must perform the request's private ready/release FIFO handshake (or a custom
callback must call :meth:`IsolatedTrialRequest.notify_guest_launched`) immediately before its
work begins. In both cases the callback must honour ``request.include_in_model == False`` by using
no normal ``MetricsSink``. Keeping process execution behind this callback lets the CLI reuse
dagrun's cgroup, timeout, cmdtype, and inner-width machinery without duplicating it here.
"""

from __future__ import annotations

import errno
import json
import math
import os
import select
import shutil
import signal
import stat as statlib
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path
from typing import BinaryIO, Protocol, cast

from dagrun.estimates import StepSpeedup
from dagrun.perflog import container_class as current_container_class
from dagrun.perflog import machine_id as current_machine_id

__all__ = [
    "CaptureArtifact",
    "CaptureConfig",
    "CaptureKind",
    "CaptureManifest",
    "CaptureState",
    "CaptureTrialRecord",
    "CaptureWindow",
    "GuestLaunch",
    "GuestLaunchSignal",
    "IsolatedTrialRequest",
    "IsolatedTrialResult",
    "ProfileCaptureError",
    "RunIsolatedTrial",
    "SweetSpotSelection",
    "ToolPreflight",
    "capture_at_sweet_spot",
    "centered_window",
    "preflight_capture_tools",
    "select_sweet_spot",
]

_MANIFEST_SCHEMA = "dagrun-profile-capture-v1"
_DEFAULT_WPROF_WINDOW_S = 0.4
_MAX_DIAGNOSTIC_CHARS = 4096
_WPROF_READY_MARKER = b"Running in flight recorder mode"
_WPROF_PID_PREFIX = b"DAGRUN_WPROF_PID="
_WPROF_PRIVILEGED_EXEC = (
    'gate=$1; shift; printf "DAGRUN_WPROF_PID=%s\\n" "$$" >&2; '
    'if ! IFS= read -r _ < "$gate"; then exit 125; fi; exec "$@"'
)
_PIDFD_SIGNAL_HELPER = (
    "import signal,sys; "
    "signal.pidfd_send_signal(0, getattr(signal, 'SIG' + sys.argv[1]))"
)
_PROCESS_GROUP_SIGNAL_HELPER = (
    "import os,signal,sys; "
    "\ntry: os.killpg(int(sys.argv[1]), getattr(signal, 'SIG' + sys.argv[2]))"
    "\nexcept ProcessLookupError: pass"
)


class CaptureKind(str, Enum):
    """Supported expensive profiler modes."""

    PERF = "perf"
    WPROF = "wprof"


class CaptureState(str, Enum):
    """Stable states written to capture manifests."""

    RUNNING = "running"
    COMPLETE = "complete"
    FAILED = "failed"
    SKIPPED = "skipped"


@dataclass(frozen=True)
class SweetSpotSelection:
    """The economic width selected from an uninstrumented scaling model.

    ``expected_wall_s`` drives placement of a profiling window in the middle of the next trial.
    Prefer the raw measured wall at the recommended width over the contention-adjusted model wall
    for this purpose: the timer runs in real wall-clock time.
    """

    step: str
    workload_digest: str
    inner_jobs: int
    expected_wall_s: float
    baseline_inner_jobs: int
    speedup: float
    model_wall_s: float
    raw_wall_s: float | None
    source: str = "scaling-model-economic-plateau"
    git_sha: str = ""

    def __post_init__(self) -> None:
        if not self.step:
            raise ValueError("sweet-spot selection needs a step tag")
        if self.inner_jobs < 1 or self.baseline_inner_jobs < 1:
            raise ValueError("sweet-spot widths must be positive")
        for name, value in (
            ("expected_wall_s", self.expected_wall_s),
            ("speedup", self.speedup),
            ("model_wall_s", self.model_wall_s),
        ):
            if not math.isfinite(value) or value <= 0.0:
                raise ValueError(f"{name} must be finite and positive")
        if self.raw_wall_s is not None and (
            not math.isfinite(self.raw_wall_s) or self.raw_wall_s <= 0.0
        ):
            raise ValueError("raw_wall_s must be finite and positive when present")

    def to_dict(self) -> dict[str, object]:
        """Encode the selection using the stable capture-manifest field names."""

        return {
            "step": self.step,
            "workload_digest": self.workload_digest,
            "inner_jobs": self.inner_jobs,
            "expected_wall_s": self.expected_wall_s,
            "baseline_inner_jobs": self.baseline_inner_jobs,
            "speedup": self.speedup,
            "model_wall_s": self.model_wall_s,
            "raw_wall_s": self.raw_wall_s,
            "source": self.source,
            "git_sha": self.git_sha,
        }


def select_sweet_spot(
    speedup: StepSpeedup,
    *,
    workload_digest: str,
    git_sha: str = "",
) -> SweetSpotSelection:
    """Turn a fitted :class:`StepSpeedup` recommendation into capture provenance."""

    level = next(
        (
            candidate
            for candidate in speedup.levels
            if candidate.inner_jobs == speedup.recommended_inner_jobs
        ),
        None,
    )
    if level is None:
        raise ValueError(
            f"recommended width {speedup.recommended_inner_jobs} is absent from "
            f"the scaling curve for {speedup.step!r}"
        )
    expected = level.raw_wall_s if level.raw_wall_s is not None else level.wall_s
    return SweetSpotSelection(
        step=speedup.step,
        workload_digest=workload_digest,
        inner_jobs=speedup.recommended_inner_jobs,
        expected_wall_s=expected,
        baseline_inner_jobs=speedup.baseline_inner_jobs,
        speedup=level.speedup,
        model_wall_s=level.wall_s,
        raw_wall_s=level.raw_wall_s,
        git_sha=git_sha,
    )


@dataclass(frozen=True)
class CaptureWindow:
    """A centred half-open profiling interval relative to trial launch."""

    requested_duration_s: float
    start_offset_s: float
    duration_s: float
    clipped: bool

    @property
    def end_offset_s(self) -> float:
        """Return the offset at which collection should stop."""

        return self.start_offset_s + self.duration_s

    def to_dict(self) -> dict[str, object]:
        """Encode the window using the stable capture-manifest field names."""

        return {
            "requested_duration_s": self.requested_duration_s,
            "start_offset_s": self.start_offset_s,
            "duration_s": self.duration_s,
            "end_offset_s": self.end_offset_s,
            "clipped": self.clipped,
        }


def centered_window(expected_wall_s: float, requested_duration_s: float) -> CaptureWindow:
    """Place a window at the trial midpoint while preserving 10% edges when possible.

    Very short steps cannot accommodate the requested duration away from both startup and
    shutdown.  In that case the duration is clipped to 80% of the expected wall, still leaving a
    10% margin at each edge, and the manifest makes that loss of fidelity explicit.
    """

    if not math.isfinite(expected_wall_s) or expected_wall_s <= 0.0:
        raise ValueError("expected wall time must be finite and positive")
    if not math.isfinite(requested_duration_s) or requested_duration_s <= 0.0:
        raise ValueError("capture duration must be finite and positive")
    maximum = expected_wall_s * 0.8
    actual = min(requested_duration_s, maximum)
    return CaptureWindow(
        requested_duration_s=requested_duration_s,
        start_offset_s=(expected_wall_s - actual) / 2.0,
        duration_s=actual,
        clipped=actual < requested_duration_s,
    )


@dataclass(frozen=True)
class GuestLaunch:
    """The release boundary immediately before one isolated guest begins executing."""

    step: str
    pid: int
    monotonic_s: float


class GuestLaunchSignal:
    """One-shot cross-thread notification anchoring a profiler window to guest launch."""

    def __init__(self) -> None:
        self._event = threading.Event()
        self._lock = threading.Lock()
        self._launch: GuestLaunch | None = None

    def notify(self, step: str, pid: int, monotonic_s: float) -> None:
        """Publish the one permitted guest-launch observation."""

        if not step or pid <= 0 or not math.isfinite(monotonic_s):
            raise ValueError("invalid guest-launch notification")
        with self._lock:
            if self._launch is not None:
                raise RuntimeError("guest launch was reported more than once")
            self._launch = GuestLaunch(step=step, pid=pid, monotonic_s=monotonic_s)
            self._event.set()

    def wait(self, timeout_s: float) -> GuestLaunch | None:
        """Wait up to ``timeout_s`` for the guest-launch observation."""

        if not self._event.wait(timeout=max(0.0, timeout_s)):
            return None
        with self._lock:
            return self._launch

    @property
    def notified(self) -> bool:
        """Whether the one-shot launch boundary has already been reported."""

        return self._event.is_set()


@dataclass(frozen=True)
class CaptureConfig:
    """Opt-in capture policy.

    ``sudo`` is an argv prefix, normally ``("sudo", "-n")`` when the operator explicitly wants
    privileged collection.  It is empty by default: this module never escalates privileges merely
    because a profiler is installed.  Tool-specific extra arguments are argv tokens, not shell
    fragments.
    """

    output_dir: Path
    capture_perf: bool = False
    wprof_windows: int = 0
    perf_window_s: float | None = None
    wprof_window_s: float = _DEFAULT_WPROF_WINDOW_S
    sudo: tuple[str, ...] = ()
    perf_binary: str = "perf"
    wprof_binary: str = "wprof"
    perf_args: tuple[str, ...] = ("--call-graph", "dwarf")
    wprof_args: tuple[str, ...] = ()
    preflight_timeout_s: float = 10.0
    control_ack_timeout_s: float = 2.0
    wprof_ready_timeout_s: float = 10.0
    profiler_exit_grace_s: float = 2.0

    def __post_init__(self) -> None:
        object.__setattr__(self, "output_dir", Path(self.output_dir))
        if self.wprof_windows < 0:
            raise ValueError("wprof_windows must be non-negative")
        if self.perf_window_s is not None and (
            not math.isfinite(self.perf_window_s) or self.perf_window_s <= 0.0
        ):
            raise ValueError("perf_window_s must be finite and positive when present")
        for name, value in (
            ("wprof_window_s", self.wprof_window_s),
            ("preflight_timeout_s", self.preflight_timeout_s),
            ("control_ack_timeout_s", self.control_ack_timeout_s),
            ("wprof_ready_timeout_s", self.wprof_ready_timeout_s),
            ("profiler_exit_grace_s", self.profiler_exit_grace_s),
        ):
            if not math.isfinite(value) or value <= 0.0:
                raise ValueError(f"{name} must be finite and positive")
        if not self.perf_binary or not self.wprof_binary:
            raise ValueError("profiler binary names must be non-empty")

    @property
    def requested_kinds(self) -> tuple[CaptureKind, ...]:
        """Return requested profiler kinds in stable manifest order."""

        kinds: list[CaptureKind] = []
        if self.capture_perf:
            kinds.append(CaptureKind.PERF)
        if self.wprof_windows:
            kinds.append(CaptureKind.WPROF)
        return tuple(kinds)


@dataclass(frozen=True)
class ToolPreflight:
    """Availability and a real minimal-capture probe for one profiler."""

    kind: CaptureKind
    requested_binary: str
    resolved_binary: str | None
    usable: bool
    returncode: int | None
    version: str
    diagnostic: str
    sudo: tuple[str, ...]

    def to_dict(self) -> dict[str, object]:
        """Encode this probe result for a capture manifest."""

        return {
            "kind": self.kind.value,
            "requested_binary": self.requested_binary,
            "resolved_binary": self.resolved_binary,
            "usable": self.usable,
            "returncode": self.returncode,
            "version": self.version,
            "diagnostic": self.diagnostic,
            "sudo": list(self.sudo),
        }


@dataclass(frozen=True)
class IsolatedTrialRequest:
    """Instructions passed to the caller's isolated-step callback.

    The callback must run one fresh copy of ``step`` at ``inner_jobs``.  For perf it must prepend
    ``argv_prefix`` to the executable argv; for wprof the sidecar is already running and the prefix
    is empty.  The actual guest must block across ``guest_launch_ready_path`` and
    ``guest_launch_release_path`` immediately before starting, or a custom callback must invoke
    :meth:`notify_guest_launched` at that same boundary. ``include_in_model`` is intentionally not
    configurable.
    """

    trial_id: str
    step: str
    inner_jobs: int
    kind: CaptureKind
    output_dir: Path
    expected_wall_s: float
    window: CaptureWindow
    argv_prefix: tuple[str, ...] = ()
    env: Mapping[str, str] = field(default_factory=dict)
    guest_launch: GuestLaunchSignal = field(default_factory=GuestLaunchSignal, repr=False)
    include_in_model: bool = field(init=False, default=False)

    def notify_guest_launched(self, step: str, pid: int, monotonic_s: float) -> None:
        """Notify capture control immediately after the isolated guest's successful spawn."""

        if step != self.step:
            raise ValueError(
                f"guest-launch step {step!r} does not match capture step {self.step!r}"
            )
        self.guest_launch.notify(step, pid, monotonic_s)

    @property
    def guest_launch_ready_path(self) -> Path:
        """Private FIFO the actual guest writes immediately before it is released."""

        return self.output_dir / "guest-launch-ready.fifo"

    @property
    def guest_launch_release_path(self) -> Path:
        """Private FIFO which releases the guest after capture clocks are anchored."""

        return self.output_dir / "guest-launch-release.fifo"


@dataclass(frozen=True)
class IsolatedTrialResult:
    """Minimal result returned by the isolated-step callback."""

    returncode: int | None
    wall_s: float
    detail: str = ""

    def __post_init__(self) -> None:
        if not math.isfinite(self.wall_s) or self.wall_s < 0.0:
            raise ValueError("trial wall_s must be finite and non-negative")

    @property
    def ok(self) -> bool:
        """Whether the observed outer command completed successfully."""

        return self.returncode == 0


class RunIsolatedTrial(Protocol):
    """Callback that runs one isolated, explicitly non-modelled step trial."""

    def __call__(self, request: IsolatedTrialRequest) -> IsolatedTrialResult: ...


@dataclass(frozen=True)
class CaptureArtifact:
    """One retained private output, named relative to the capture directory."""

    role: str
    path: str
    size_bytes: int
    mode: str

    def to_dict(self) -> dict[str, object]:
        """Encode this retained artifact for a capture manifest."""

        return {
            "role": self.role,
            "path": self.path,
            "size_bytes": self.size_bytes,
            "mode": self.mode,
        }


@dataclass(frozen=True)
class CaptureTrialRecord:
    """Manifest record for one separate recommended-width execution."""

    trial_id: str
    kind: CaptureKind
    state: CaptureState
    inner_jobs: int
    expected_wall_s: float
    window: CaptureWindow
    started_at: str
    finished_at: str
    measured_wall_s: float | None
    workload_returncode: int | None
    profiler_returncode: int | None
    argv_prefix: tuple[str, ...]
    artifacts: tuple[CaptureArtifact, ...]
    error: str
    included_in_model: bool = field(init=False, default=False)

    def to_dict(self) -> dict[str, object]:
        """Encode this instrumented trial for a capture manifest."""

        return {
            "trial_id": self.trial_id,
            "kind": self.kind.value,
            "state": self.state.value,
            "inner_jobs": self.inner_jobs,
            "expected_wall_s": self.expected_wall_s,
            "window": self.window.to_dict(),
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "measured_wall_s": self.measured_wall_s,
            "workload_returncode": self.workload_returncode,
            "profiler_returncode": self.profiler_returncode,
            "argv_prefix": list(self.argv_prefix),
            "artifacts": [artifact.to_dict() for artifact in self.artifacts],
            "error": self.error,
            "included_in_model": self.included_in_model,
        }


@dataclass(frozen=True)
class CaptureManifest:
    """Complete, retained account of a post-sweep capture session."""

    path: Path
    capture_id: str
    state: CaptureState
    created_at: str
    finished_at: str
    selection: SweetSpotSelection
    preflight: tuple[ToolPreflight, ...]
    trials: tuple[CaptureTrialRecord, ...]
    errors: tuple[str, ...]
    machine_id: str = ""
    container_class: str = ""

    def to_dict(self) -> dict[str, object]:
        """Encode the complete interoperable capture-manifest document."""

        return {
            "schema": _MANIFEST_SCHEMA,
            "machine_id": self.machine_id,
            "container_class": self.container_class,
            "capture_id": self.capture_id,
            "state": self.state.value,
            "created_at": self.created_at,
            "finished_at": self.finished_at,
            "artifact_root": ".",
            "selection": self.selection.to_dict(),
            "preflight": [result.to_dict() for result in self.preflight],
            "trials": [trial.to_dict() for trial in self.trials],
            "errors": list(self.errors),
        }


class ProfileCaptureError(RuntimeError):
    """A requested profiler capture failed after its manifest was safely retained."""

    def __init__(self, message: str, manifest: CaptureManifest):
        super().__init__(f"{message}; capture manifest: {manifest.path}")
        self.manifest = manifest


def _utc_now() -> str:
    return (
        datetime.now(timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


def _diagnostic(stdout: str, stderr: str) -> str:
    text = "\n".join(part.strip() for part in (stdout, stderr) if part.strip())
    return text[-_MAX_DIAGNOSTIC_CHARS:]


def _resolve_binary(binary: str) -> str | None:
    if os.path.sep in binary:
        path = Path(binary)
        return str(path.resolve()) if path.is_file() and os.access(path, os.X_OK) else None
    return shutil.which(binary)


def _private_file(path: Path) -> None:
    """Create an empty regular artifact without following or replacing any existing path."""

    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    fd = os.open(path, flags, 0o600)
    os.close(fd)


def _preflight_perf(config: CaptureConfig, scratch: Path) -> ToolPreflight:
    resolved = _resolve_binary(config.perf_binary)
    if resolved is None:
        return ToolPreflight(
            kind=CaptureKind.PERF,
            requested_binary=config.perf_binary,
            resolved_binary=None,
            usable=False,
            returncode=None,
            version="",
            diagnostic=f"executable {config.perf_binary!r} was not found",
            sudo=config.sudo,
        )
    try:
        version_run = subprocess.run(
            [resolved, "version"],
            cwd=scratch,
            text=True,
            capture_output=True,
            timeout=config.preflight_timeout_s,
            check=False,
        )
        version_diagnostic = _diagnostic(version_run.stdout, version_run.stderr)
        version = version_diagnostic.splitlines()[0] if version_diagnostic else ""
        probe = scratch / "perf-probe.data"
        _private_file(probe)
        command = [
            *config.sudo,
            resolved,
            "record",
            "--quiet",
            "--output",
            str(probe),
            *config.perf_args,
            "--",
            "/bin/true",
        ]
        run = subprocess.run(
            command,
            cwd=scratch,
            text=True,
            capture_output=True,
            timeout=config.preflight_timeout_s,
            check=False,
        )
        return ToolPreflight(
            kind=CaptureKind.PERF,
            requested_binary=config.perf_binary,
            resolved_binary=resolved,
            usable=run.returncode == 0 and probe.stat().st_size > 0,
            returncode=run.returncode,
            version=version,
            diagnostic=_diagnostic(run.stdout, run.stderr),
            sudo=config.sudo,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return ToolPreflight(
            kind=CaptureKind.PERF,
            requested_binary=config.perf_binary,
            resolved_binary=resolved,
            usable=False,
            returncode=None,
            version="",
            diagnostic=str(exc),
            sudo=config.sudo,
        )


def _wprof_probe_command(
    config: CaptureConfig,
    resolved: str,
    *,
    data_path: Path,
    trace_path: Path,
    window: CaptureWindow,
    selection: SweetSpotSelection | None,
) -> tuple[str, ...]:
    activate = "@now" if window.start_offset_s <= 0.0 else f"+{window.start_offset_s:.6f}s"
    command = [
        *config.sudo,
        resolved,
        "--record",
        "--prepare=@now",
        f"--activate={activate}",
        f"--dur={window.duration_s:.6f}s",
        f"--data={data_path}",
        f"--trace={trace_path}",
    ]
    if selection is not None:
        command.extend(
            (
                f"--metadata=dagrun.step={selection.step}",
                f"--metadata=dagrun.inner_jobs={selection.inner_jobs}",
                f"--metadata=dagrun.workload_digest={selection.workload_digest}",
            )
        )
    command.extend(config.wprof_args)
    return tuple(command)


def _wprof_flight_command(
    config: CaptureConfig,
    resolved: str,
    *,
    data_path: Path,
    trace_path: Path,
    window: CaptureWindow,
    selection: SweetSpotSelection,
    launch_gate: Path | None = None,
) -> tuple[str, ...]:
    """Build a rolling capture which is stopped at the desired window end.

    Wprof's delayed ``--activate`` clock includes BPF preparation. On a short step preparation can
    consume the entire delay before the workload is even launched. Flight-recorder mode instead
    completes preparation first, announces readiness, and continuously retains only the requested
    trailing interval. Sending SIGINT at ``window.end_offset_s`` therefore yields the centered
    window without racing setup.

    A privileged wprof is a child of sudo rather than the process returned by ``Popen``. The fixed
    shell wrapper prints the PID it will retain across ``exec`` and then blocks on a private gate.
    The parent opens a pidfd for that still-live shell before releasing it to exec wprof, closing
    the otherwise tiny marker-to-pidfd PID-reuse race.
    """

    wprof = [
        resolved,
        "--record",
        f"--flight-record={window.duration_s:.6f}s",
        f"--data={data_path}",
        f"--trace={trace_path}",
        f"--metadata=dagrun.step={selection.step}",
        f"--metadata=dagrun.inner_jobs={selection.inner_jobs}",
        f"--metadata=dagrun.workload_digest={selection.workload_digest}",
        *config.wprof_args,
    ]
    if not config.sudo:
        return tuple(wprof)
    if launch_gate is None:
        raise ValueError("privileged wprof requires a private exec gate")
    return (
        *config.sudo,
        "/bin/sh",
        "-c",
        _WPROF_PRIVILEGED_EXEC,
        "dagrun-wprof",
        str(launch_gate),
        *wprof,
    )


def _preflight_wprof(config: CaptureConfig, scratch: Path) -> ToolPreflight:
    resolved = _resolve_binary(config.wprof_binary)
    if resolved is None:
        return ToolPreflight(
            kind=CaptureKind.WPROF,
            requested_binary=config.wprof_binary,
            resolved_binary=None,
            usable=False,
            returncode=None,
            version="",
            diagnostic=f"executable {config.wprof_binary!r} was not found",
            sudo=config.sudo,
        )
    data_path = scratch / "wprof-probe.data"
    trace_path = scratch / "wprof-probe.pb"
    _private_file(data_path)
    _private_file(trace_path)
    probe_window = CaptureWindow(0.001, 0.0, 0.001, False)
    command = _wprof_probe_command(
        config,
        resolved,
        data_path=data_path,
        trace_path=trace_path,
        window=probe_window,
        selection=None,
    )
    try:
        version_run = subprocess.run(
            [resolved, "--version"],
            cwd=scratch,
            text=True,
            capture_output=True,
            timeout=config.preflight_timeout_s,
            check=False,
        )
        version_diagnostic = _diagnostic(version_run.stdout, version_run.stderr)
        version = version_diagnostic.splitlines()[0] if version_diagnostic else ""
        run = subprocess.run(
            command,
            cwd=scratch,
            text=True,
            capture_output=True,
            timeout=config.preflight_timeout_s,
            check=False,
        )
        usable = (
            run.returncode == 0
            and data_path.stat().st_size > 0
            and trace_path.stat().st_size > 0
        )
        return ToolPreflight(
            kind=CaptureKind.WPROF,
            requested_binary=config.wprof_binary,
            resolved_binary=resolved,
            usable=usable,
            returncode=run.returncode,
            version=version,
            diagnostic=_diagnostic(run.stdout, run.stderr),
            sudo=config.sudo,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return ToolPreflight(
            kind=CaptureKind.WPROF,
            requested_binary=config.wprof_binary,
            resolved_binary=resolved,
            usable=False,
            returncode=None,
            version="",
            diagnostic=str(exc),
            sudo=config.sudo,
        )


def preflight_capture_tools(config: CaptureConfig) -> dict[CaptureKind, ToolPreflight]:
    """Probe every requested profiler without silently adding privilege escalation."""

    config.output_dir.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix=".capture-preflight-", dir=config.output_dir))
    os.chmod(scratch, 0o700)
    try:
        results: dict[CaptureKind, ToolPreflight] = {}
        if config.capture_perf:
            results[CaptureKind.PERF] = _preflight_perf(config, scratch)
        if config.wprof_windows:
            results[CaptureKind.WPROF] = _preflight_wprof(config, scratch)
        return results
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def _safe_component(value: str) -> str:
    safe = "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in value)
    return safe[:80] or "step"


def _new_capture_dir(config: CaptureConfig, selection: SweetSpotSelection) -> tuple[str, Path]:
    captures = config.output_dir / "captures"
    captures.mkdir(parents=True, exist_ok=True)
    stem = f"{_safe_component(selection.step)}-{datetime.now(timezone.utc):%Y%m%dT%H%M%SZ}-"
    directory = Path(tempfile.mkdtemp(prefix=stem, dir=captures))
    os.chmod(directory, 0o700)
    return directory.name, directory


def _write_manifest(manifest: CaptureManifest) -> None:
    manifest.path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".manifest-", suffix=".json", dir=manifest.path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(manifest.to_dict(), handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, manifest.path)
        os.chmod(manifest.path, 0o600)
    except BaseException:
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def _artifact(root: Path, path: Path, role: str) -> CaptureArtifact | None:
    try:
        stat = path.stat()
        if stat.st_mode & 0o777 != 0o600:
            os.chmod(path, 0o600)
            stat = path.stat()
    except OSError:
        return None
    if not path.is_file():
        return None
    return CaptureArtifact(
        role=role,
        path=str(path.relative_to(root)),
        size_bytes=stat.st_size,
        mode=f"{stat.st_mode & 0o777:04o}",
    )


def _secure_artifacts(
    paths: Sequence[Path], sudo: tuple[str, ...], timeout_s: float
) -> str:
    """Reclaim exact profiler outputs and verify regular, caller-owned mode-0600 files.

    Perf and wprof may unlink a pre-created destination and replace it. Under explicit sudo that
    replacement is root-owned and commonly mode 0644, so the caller cannot repair it directly.
    Reclaim only the exact known paths through the same non-interactive privilege prefix, then do
    the final chmod and verification without privilege. The containing trial directory is 0700
    throughout, so even the brief root-owned mode-0644 state is not traversable by other users.
    """

    existing: list[Path] = []
    for path in paths:
        try:
            metadata = path.lstat()
        except OSError:
            continue
        if not statlib.S_ISREG(metadata.st_mode):
            return f"profiler artifact is not a regular file: {path}"
        existing.append(path)
    if sudo and existing:
        owner = f"{os.getuid()}:{os.getgid()}"
        try:
            reclaimed = subprocess.run(
                [
                    *sudo,
                    "/bin/chown",
                    "--no-dereference",
                    owner,
                    "--",
                    *(str(path) for path in existing),
                ],
                text=True,
                capture_output=True,
                timeout=timeout_s,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            return f"could not reclaim privileged profiler artifacts: {exc}"
        if reclaimed.returncode != 0:
            detail = _diagnostic(reclaimed.stdout, reclaimed.stderr)
            return (
                f"could not reclaim privileged profiler artifacts (exit "
                f"{reclaimed.returncode})"
                + (f": {detail}" if detail else "")
            )
    for path in existing:
        try:
            os.chmod(path, 0o600)
            metadata = path.lstat()
        except OSError as exc:
            return f"could not secure profiler artifact {path}: {exc}"
        if not statlib.S_ISREG(metadata.st_mode):
            return f"profiler artifact is not a regular file: {path}"
        if metadata.st_uid != os.getuid() or metadata.st_gid != os.getgid():
            return f"profiler artifact ownership was not reclaimed: {path}"
        if metadata.st_mode & 0o777 != 0o600:
            return f"profiler artifact mode is not 0600: {path}"
    return ""


@dataclass
class _GuestLaunchOutcome:
    released: bool = False
    error: str = ""


class _GuestLaunchBridge:
    """Translate a shell-level FIFO handshake into an in-process launch timestamp.

    The guest writes its PID only after scheduler setup (including its cgroup self-move) and then
    blocks on the release FIFO.  We pin the monotonic origin while it is blocked and release it
    only after every profiler controller can observe that origin.  A callback used by a library
    test may instead call :meth:`IsolatedTrialRequest.notify_guest_launched` directly; in that
    mode the bridge exits without requiring either FIFO to be opened.
    """

    def __init__(self, request: IsolatedTrialRequest, timeout_s: float) -> None:
        self.request = request
        self.timeout_s = timeout_s
        self.outcome = _GuestLaunchOutcome()
        self._finished = threading.Event()
        self._initialized = threading.Event()
        self._started = False
        self._thread = threading.Thread(
            target=self._run, name="dagrun-guest-launch", daemon=True
        )

    def start(self) -> None:
        self._thread.start()
        self._started = True
        if not self._initialized.wait(timeout=self.timeout_s):
            raise RuntimeError("guest-launch FIFO bridge did not initialize")
        if self.outcome.error:
            raise RuntimeError(self.outcome.error)

    def finish(self) -> _GuestLaunchOutcome:
        if not self._started:
            return self.outcome
        self._finished.set()
        self._thread.join(timeout=self.timeout_s + 1.0)
        if self._thread.is_alive() and not self.outcome.error:
            self.outcome.error = "guest-launch FIFO bridge did not stop"
        if (
            not self.outcome.released
            and not self.request.guest_launch.notified
            and not self.outcome.error
        ):
            self.outcome.error = "isolated callback did not report guest launch"
        return self.outcome

    def _open_release_writer(self) -> int | None:
        deadline = time.monotonic() + self.timeout_s
        while not self._finished.is_set() and time.monotonic() < deadline:
            try:
                return os.open(
                    self.request.guest_launch_release_path,
                    os.O_WRONLY | os.O_NONBLOCK,
                )
            except OSError as exc:
                if exc.errno not in (errno.ENXIO, errno.ENOENT):
                    raise
                self._finished.wait(0.005)
        return None

    def _run(self) -> None:
        ready_fd: int | None = None
        release_fd: int | None = None
        buffered = b""
        try:
            ready_fd = os.open(
                self.request.guest_launch_ready_path,
                os.O_RDONLY | os.O_NONBLOCK,
            )
            self._initialized.set()
            while not self._finished.is_set():
                if self.request.guest_launch.notified:
                    return
                readable, _, _ = select.select([ready_fd], [], [], 0.05)
                if not readable:
                    continue
                chunk = os.read(ready_fd, 256)
                if not chunk:
                    self._finished.wait(0.005)
                    continue
                buffered += chunk
                if b"\n" not in buffered:
                    if len(buffered) > 128:
                        raise ValueError("guest-launch PID marker is too long")
                    continue
                marker = buffered.splitlines()[0].strip()
                try:
                    pid = int(marker)
                except ValueError as exc:
                    raise ValueError(f"invalid guest-launch PID marker: {marker!r}") from exc
                if pid <= 0:
                    raise ValueError(f"invalid guest-launch PID marker: {marker!r}")
                release_fd = self._open_release_writer()
                if release_fd is None:
                    if not self._finished.is_set():
                        self.outcome.error = "guest did not open its launch-release FIFO"
                    return
                launched_at = time.monotonic()
                self.request.notify_guest_launched(
                    self.request.step, pid, launched_at
                )
                os.write(release_fd, b"go\n")
                self.outcome.released = True
                return
        except (OSError, ValueError, RuntimeError) as exc:
            self.outcome.error = f"guest-launch FIFO handshake failed: {exc}"
        finally:
            self._initialized.set()
            if release_fd is not None:
                os.close(release_fd)
            if ready_fd is not None:
                os.close(ready_fd)


def _prepare_guest_launch_fifos(request: IsolatedTrialRequest) -> None:
    os.mkfifo(request.guest_launch_ready_path, 0o600)
    os.mkfifo(request.guest_launch_release_path, 0o600)


@dataclass
class _PerfControlOutcome:
    enabled: bool = False
    disabled: bool = False
    error: str = ""


class _PerfWindowController:
    """Drive perf's FIFO control interface and require an ack for each transition."""

    def __init__(
        self,
        control_path: Path,
        ack_path: Path,
        window: CaptureWindow,
        guest_launch: GuestLaunchSignal,
        ack_timeout_s: float,
    ) -> None:
        self.control_path = control_path
        self.ack_path = ack_path
        self.window = window
        self.guest_launch = guest_launch
        self.ack_timeout_s = ack_timeout_s
        self.outcome = _PerfControlOutcome()
        self._finished = threading.Event()
        self._ready = threading.Event()
        self._origin: float | None = None
        self._started = False
        self._thread = threading.Thread(target=self._run, name="dagrun-perf-control", daemon=True)

    def start(self) -> None:
        self._thread.start()
        self._started = True
        if not self._ready.wait(timeout=self.ack_timeout_s):
            raise RuntimeError("perf FIFO controller did not initialize")
        if self.outcome.error:
            raise RuntimeError(self.outcome.error)

    def finish(self) -> _PerfControlOutcome:
        if not self._started:
            return self.outcome
        self._finished.set()
        self._thread.join(timeout=self.ack_timeout_s + 1.0)
        if self._thread.is_alive() and not self.outcome.error:
            self.outcome.error = "perf FIFO controller did not stop"
        return self.outcome

    def _open_control_writer(self) -> int | None:
        deadline = time.monotonic() + self.ack_timeout_s
        while not self._finished.is_set() and time.monotonic() < deadline:
            try:
                return os.open(self.control_path, os.O_WRONLY | os.O_NONBLOCK)
            except OSError as exc:
                if exc.errno not in (errno.ENXIO, errno.ENOENT):
                    raise
                self._finished.wait(0.01)
        return None

    def _wait_until(self, offset_s: float) -> bool:
        assert self._origin is not None
        remaining = self._origin + offset_s - time.monotonic()
        return not self._finished.wait(max(0.0, remaining))

    def _ack(self, ack_fd: int) -> bool:
        deadline = time.monotonic() + self.ack_timeout_s
        buffered = b""
        while time.monotonic() < deadline and not self._finished.is_set():
            readable, _, _ = select.select([ack_fd], [], [], min(0.05, deadline - time.monotonic()))
            if not readable:
                continue
            chunk = os.read(ack_fd, 4096)
            if not chunk:
                time.sleep(0.005)
                continue
            buffered += chunk
            if b"\n" in buffered:
                return buffered.splitlines()[0].strip() == b"ack"
        return False

    def _command(self, control_fd: int, ack_fd: int, command: bytes) -> bool:
        os.write(control_fd, command + b"\n")
        return self._ack(ack_fd)

    def _run(self) -> None:
        ack_fd: int | None = None
        control_fd: int | None = None
        try:
            ack_fd = os.open(self.ack_path, os.O_RDONLY | os.O_NONBLOCK)
            self._ready.set()
            launch: GuestLaunch | None = None
            while launch is None:
                if self._finished.is_set():
                    self.outcome.error = "isolated callback did not report guest launch"
                    return
                launch = self.guest_launch.wait(0.05)
            self._origin = launch.monotonic_s
            control_fd = self._open_control_writer()
            if control_fd is None:
                if not self._finished.is_set():
                    self.outcome.error = "perf did not open its control FIFO"
                return
            if not self._wait_until(self.window.start_offset_s):
                self.outcome.error = "step ended before the perf window opened"
                return
            if not self._command(control_fd, ack_fd, b"enable"):
                self.outcome.error = "perf did not acknowledge enable"
                return
            self.outcome.enabled = True
            if not self._wait_until(self.window.end_offset_s):
                self.outcome.error = "step ended before the perf window closed"
                return
            if not self._command(control_fd, ack_fd, b"disable"):
                self.outcome.error = "perf did not acknowledge disable"
                return
            self.outcome.disabled = True
        except OSError as exc:
            self.outcome.error = f"perf FIFO control failed: {exc}"
        finally:
            self._ready.set()
            if control_fd is not None:
                os.close(control_fd)
            if ack_fd is not None:
                os.close(ack_fd)


def _perf_trial(
    capture_root: Path,
    selection: SweetSpotSelection,
    config: CaptureConfig,
    preflight: ToolPreflight,
    run_trial: RunIsolatedTrial,
) -> CaptureTrialRecord:
    trial_id = "perf-001"
    trial_dir = capture_root / trial_id
    trial_dir.mkdir(mode=0o700)
    os.chmod(trial_dir, 0o700)
    perf_data = trial_dir / "perf.data"
    control_path = trial_dir / "control.fifo"
    ack_path = trial_dir / "ack.fifo"
    _private_file(perf_data)
    os.mkfifo(control_path, 0o600)
    os.mkfifo(ack_path, 0o600)
    perf_duration = (
        config.perf_window_s
        if config.perf_window_s is not None
        else selection.expected_wall_s * 0.8
    )
    window = centered_window(selection.expected_wall_s, perf_duration)
    resolved = preflight.resolved_binary
    assert resolved is not None
    prefix = (
        *config.sudo,
        resolved,
        "record",
        "--quiet",
        "--output",
        str(perf_data),
        "--control",
        f"fifo:{control_path},{ack_path}",
        "--delay=-1",
        *config.perf_args,
        "--",
    )
    request = IsolatedTrialRequest(
        trial_id=trial_id,
        step=selection.step,
        inner_jobs=selection.inner_jobs,
        kind=CaptureKind.PERF,
        output_dir=trial_dir,
        expected_wall_s=selection.expected_wall_s,
        window=window,
        argv_prefix=prefix,
    )
    _prepare_guest_launch_fifos(request)
    started_at = _utc_now()
    launch_bridge = _GuestLaunchBridge(request, config.control_ack_timeout_s)
    controller = _PerfWindowController(
        control_path,
        ack_path,
        window,
        request.guest_launch,
        config.control_ack_timeout_s,
    )
    result: IsolatedTrialResult | None = None
    error = ""
    fatal: BaseException | None = None
    try:
        launch_bridge.start()
        controller.start()
        result = run_trial(request)
    except BaseException as exc:
        error = f"isolated perf trial failed: {exc}"
        fatal = exc
    launch = launch_bridge.finish()
    control = controller.finish()
    if fatal is not None and not isinstance(fatal, Exception):
        raise fatal
    if control.error:
        error = "; ".join(part for part in (error, control.error) if part)
    if launch.error and launch.error not in error:
        error = "; ".join(part for part in (error, launch.error) if part)
    security_error = _secure_artifacts(
        (perf_data,), config.sudo, config.profiler_exit_grace_s
    )
    if security_error:
        error = "; ".join(part for part in (error, security_error) if part)
    artifact = _artifact(capture_root, perf_data, "perf-data")
    if artifact is None or artifact.size_bytes == 0:
        error = "; ".join(part for part in (error, "perf produced no data") if part)
    if result is not None and not result.ok:
        detail = f": {result.detail}" if result.detail else ""
        status = "with unavailable status" if result.returncode is None else f"{result.returncode}"
        error = "; ".join(
            part
            for part in (error, f"perf wrapper exited {status}{detail}")
            if part
        )
    complete = result is not None and result.ok and control.enabled and control.disabled and not error
    return CaptureTrialRecord(
        trial_id=trial_id,
        kind=CaptureKind.PERF,
        state=CaptureState.COMPLETE if complete else CaptureState.FAILED,
        inner_jobs=selection.inner_jobs,
        expected_wall_s=selection.expected_wall_s,
        window=window,
        started_at=started_at,
        finished_at=_utc_now(),
        measured_wall_s=None if result is None else result.wall_s,
        workload_returncode=None,
        profiler_returncode=None if result is None else result.returncode,
        argv_prefix=prefix,
        artifacts=() if artifact is None else (artifact,),
        error=error,
    )


class _WprofLogPump:
    """Persist combined wprof output while recognizing readiness and the privileged PID."""

    def __init__(self, source: BinaryIO, log: BinaryIO, expect_pid_marker: bool) -> None:
        self.source = source
        self.log = log
        self.expect_pid_marker = expect_pid_marker
        self.ready = threading.Event()
        self.pid_announced = threading.Event()
        self.done = threading.Event()
        self.profiler_pid: int | None = None
        self.error = ""
        self._thread = threading.Thread(target=self._run, name="dagrun-wprof-log", daemon=True)

    def start(self) -> None:
        self._thread.start()

    def join(self, timeout_s: float) -> None:
        self._thread.join(timeout=timeout_s)
        if self._thread.is_alive() and not self.error:
            self.error = "wprof log reader did not stop"

    def _run(self) -> None:
        probe = b""
        try:
            while chunk := os.read(self.source.fileno(), 8192):
                self.log.write(chunk)
                self.log.flush()
                probe = (probe + chunk)[-16_384:]
                if _WPROF_READY_MARKER in probe:
                    self.ready.set()
                if self.expect_pid_marker and self.profiler_pid is None:
                    for line in probe.splitlines():
                        if not line.startswith(_WPROF_PID_PREFIX):
                            continue
                        token = line[len(_WPROF_PID_PREFIX) :].strip()
                        try:
                            pid = int(token)
                        except ValueError:
                            self.error = f"invalid privileged wprof PID marker: {line!r}"
                        else:
                            if pid > 0:
                                self.profiler_pid = pid
                                self.pid_announced.set()
                            else:
                                self.error = f"invalid privileged wprof PID marker: {line!r}"
                        break
        except OSError as exc:
            self.error = f"could not read wprof output: {exc}"
        finally:
            self.done.set()


@dataclass
class _PinnedProcess:
    """A process identity which remains exact even after its numeric PID can be reused."""

    pid: int
    pidfd: int
    privileged: bool

    def close(self) -> None:
        try:
            os.close(self.pidfd)
        except OSError:
            pass


def _pin_process(pid: int, *, privileged: bool) -> tuple[_PinnedProcess | None, str]:
    try:
        pidfd = os.pidfd_open(pid)
    except (AttributeError, OSError) as exc:
        return None, f"could not pin wprof process identity for PID {pid}: {exc}"
    return _PinnedProcess(pid=pid, pidfd=pidfd, privileged=privileged), ""


def _pidfd_exited(identity: _PinnedProcess, timeout_s: float) -> bool:
    readable, _, _ = select.select([identity.pidfd], [], [], max(0.0, timeout_s))
    return bool(readable)


def _wait_for_wprof_pid(
    supervisor: _PinnedProcess, pump: _WprofLogPump, timeout_s: float
) -> str:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if pump.profiler_pid is not None:
            return ""
        if pump.error:
            return pump.error
        if pump.done.is_set() or _pidfd_exited(supervisor, 0.0):
            return "privileged wprof exited before reporting its exec PID"
        pump.pid_announced.wait(min(0.02, max(0.0, deadline - time.monotonic())))
    return f"privileged wprof did not report its exec PID within {timeout_s:.3f}s"


def _release_wprof_gate(path: Path, timeout_s: float) -> str:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as exc:
            if exc.errno == errno.ENXIO:
                time.sleep(0.005)
                continue
            return f"could not open privileged wprof exec gate: {exc}"
        try:
            os.write(fd, b"go\n")
        except OSError as exc:
            return f"could not release privileged wprof exec gate: {exc}"
        finally:
            os.close(fd)
        return ""
    return f"privileged wprof did not wait on its exec gate within {timeout_s:.3f}s"


def _wait_for_wprof_ready(
    supervisor: _PinnedProcess, pump: _WprofLogPump, timeout_s: float
) -> str:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if pump.ready.is_set():
            return ""
        if pump.error:
            return pump.error
        if pump.done.is_set() or _pidfd_exited(supervisor, 0.0):
            break
        wait_s = min(0.02, max(0.0, deadline - time.monotonic()))
        pump.ready.wait(timeout=wait_s)
    if pump.done.is_set() or _pidfd_exited(supervisor, 0.0):
        return "wprof exited before announcing flight-recorder readiness"
    return f"wprof did not announce flight-recorder readiness within {timeout_s:.3f}s"


def _send_pinned_wprof_signal(
    config: CaptureConfig,
    identity: _PinnedProcess,
    signal_name: str,
    timeout_s: float,
) -> str:
    """Signal the pinned wprof task, never a reusable numeric PID."""

    if identity.privileged:
        try:
            sent = subprocess.run(
                [
                    *config.sudo,
                    sys.executable,
                    "-c",
                    _PIDFD_SIGNAL_HELPER,
                    signal_name,
                ],
                stdin=identity.pidfd,
                text=True,
                capture_output=True,
                timeout=timeout_s,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            return f"could not send SIG{signal_name} to wprof: {exc}"
        if sent.returncode != 0:
            detail = _diagnostic(sent.stdout, sent.stderr)
            return (
                f"could not send SIG{signal_name} to wprof (exit {sent.returncode})"
                + (f": {detail}" if detail else "")
            )
        return ""
    try:
        signal.pidfd_send_signal(
            identity.pidfd, getattr(signal, f"SIG{signal_name}")
        )
    except OSError as exc:
        return f"could not send SIG{signal_name} to wprof: {exc}"
    return ""


def _send_wprof_group_signal(
    config: CaptureConfig, pgid: int, signal_name: str, timeout_s: float
) -> str:
    """Signal the owned profiler session while its direct leader remains unreaped.

    Keeping the direct child unreaped reserves the numeric session/process-group id, so this group
    operation cannot hit a later unrelated process group.  Missing groups are successful cleanup.
    """

    if config.sudo:
        try:
            sent = subprocess.run(
                [
                    *config.sudo,
                    sys.executable,
                    "-c",
                    _PROCESS_GROUP_SIGNAL_HELPER,
                    str(pgid),
                    signal_name,
                ],
                stdin=subprocess.DEVNULL,
                text=True,
                capture_output=True,
                timeout=timeout_s,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as exc:
            return f"could not send SIG{signal_name} to wprof process group: {exc}"
        if sent.returncode != 0:
            detail = _diagnostic(sent.stdout, sent.stderr)
            return (
                f"could not send SIG{signal_name} to wprof process group "
                f"(exit {sent.returncode})" + (f": {detail}" if detail else "")
            )
        return ""
    try:
        os.killpg(pgid, getattr(signal, f"SIG{signal_name}"))
    except ProcessLookupError:
        return ""
    except OSError as exc:
        return f"could not send SIG{signal_name} to wprof process group: {exc}"
    return ""


def _teardown_wprof_group(
    process: subprocess.Popen[bytes],
    supervisor: _PinnedProcess | None,
    config: CaptureConfig,
    *,
    graceful_interrupt_sent: bool,
) -> str:
    """Reap wprof and every descendant without ever signalling a reusable PID."""

    errors: list[str] = []
    if (
        graceful_interrupt_sent
        and supervisor is not None
        and not _pidfd_exited(supervisor, config.profiler_exit_grace_s)
    ):
        errors.append("wprof did not exit after SIGINT")

    # The direct child has deliberately not been poll()/wait()ed, so its PID still safely owns
    # this fresh session and process group even if it is already a zombie. Sweep the group to
    # prevent a profiler helper from surviving its parent.
    term_error = _send_wprof_group_signal(
        config, process.pid, "TERM", config.profiler_exit_grace_s
    )
    if term_error:
        errors.append(term_error)
    threading.Event().wait(min(0.05, config.profiler_exit_grace_s))
    kill_error = _send_wprof_group_signal(
        config, process.pid, "KILL", config.profiler_exit_grace_s
    )
    if kill_error:
        errors.append(kill_error)
    try:
        process.wait(timeout=config.profiler_exit_grace_s)
    except subprocess.TimeoutExpired:
        errors.append("wprof process-group teardown did not reap its session leader")
    return "; ".join(errors)


@dataclass
class _WprofStopOutcome:
    sent: bool = False
    error: str = ""


class _WprofStopController:
    """Interrupt only wprof at the selected window end while the workload keeps running."""

    def __init__(
        self,
        config: CaptureConfig,
        profiler: _PinnedProcess,
        guest_launch: GuestLaunchSignal,
        end_offset_s: float,
        workload_done: threading.Event,
    ) -> None:
        self.config = config
        self.profiler = profiler
        self.guest_launch = guest_launch
        self.end_offset_s = end_offset_s
        self.workload_done = workload_done
        self.launch: GuestLaunch | None = None
        self.outcome = _WprofStopOutcome()
        self._started = False
        self._thread = threading.Thread(target=self._run, name="dagrun-wprof-stop", daemon=True)

    def start(self) -> None:
        self._thread.start()
        self._started = True

    def finish(self) -> _WprofStopOutcome:
        if not self._started:
            return self.outcome
        self._thread.join(timeout=self.config.profiler_exit_grace_s + 1.0)
        if self._thread.is_alive() and not self.outcome.error:
            self.outcome.error = "wprof stop controller did not finish"
        return self.outcome

    def _run(self) -> None:
        while self.launch is None:
            if self.workload_done.is_set():
                self.outcome.error = "isolated callback did not report guest launch"
                return
            if _pidfd_exited(self.profiler, 0.0):
                self.outcome.error = "wprof exited after readiness but before guest launch"
                return
            self.launch = self.guest_launch.wait(0.05)
        deadline = self.launch.monotonic_s + self.end_offset_s
        while True:
            if self.workload_done.is_set():
                return
            if _pidfd_exited(self.profiler, 0.0):
                self.outcome.error = "wprof exited before the selected window ended"
                return
            remaining = deadline - time.monotonic()
            if remaining <= 0.0:
                break
            self.workload_done.wait(min(0.01, remaining))
        error = _send_pinned_wprof_signal(
            self.config,
            self.profiler,
            "INT",
            self.config.profiler_exit_grace_s,
        )
        if error:
            self.outcome.error = error
            return
        self.outcome.sent = True


def _wprof_trial(
    capture_root: Path,
    index: int,
    selection: SweetSpotSelection,
    config: CaptureConfig,
    preflight: ToolPreflight,
    run_trial: RunIsolatedTrial,
) -> CaptureTrialRecord:
    trial_id = f"wprof-{index:03d}"
    trial_dir = capture_root / trial_id
    trial_dir.mkdir(mode=0o700)
    os.chmod(trial_dir, 0o700)
    data_path = trial_dir / "wprof.data"
    trace_path = trial_dir / "trace.pb"
    log_path = trial_dir / "wprof.log"
    exec_gate = trial_dir / "wprof-exec-gate.fifo"
    for path in (data_path, trace_path, log_path):
        _private_file(path)
    if config.sudo:
        os.mkfifo(exec_gate, 0o600)
    window = centered_window(selection.expected_wall_s, config.wprof_window_s)
    resolved = preflight.resolved_binary
    assert resolved is not None
    command = _wprof_flight_command(
        config,
        resolved,
        data_path=data_path,
        trace_path=trace_path,
        window=window,
        selection=selection,
        launch_gate=exec_gate if config.sudo else None,
    )
    request = IsolatedTrialRequest(
        trial_id=trial_id,
        step=selection.step,
        inner_jobs=selection.inner_jobs,
        kind=CaptureKind.WPROF,
        output_dir=trial_dir,
        expected_wall_s=selection.expected_wall_s,
        window=window,
    )
    _prepare_guest_launch_fifos(request)
    started_at = _utc_now()
    result: IsolatedTrialResult | None = None
    profiler_returncode: int | None = None
    error = ""
    process: subprocess.Popen[bytes] | None = None
    pump: _WprofLogPump | None = None
    supervisor: _PinnedProcess | None = None
    profiler: _PinnedProcess | None = None
    launch_bridge: _GuestLaunchBridge | None = None
    controller: _WprofStopController | None = None
    workload_done: threading.Event | None = None
    fatal: BaseException | None = None
    try:
        with log_path.open("wb") as log:
            try:
                process = subprocess.Popen(
                    command,
                    cwd=trial_dir,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                    umask=0o077,
                )
                assert process.stdout is not None
                supervisor, pin_error = _pin_process(process.pid, privileged=False)
                if pin_error:
                    error = pin_error
                pump = _WprofLogPump(
                    cast(BinaryIO, process.stdout),
                    log,
                    expect_pid_marker=bool(config.sudo),
                )
                pump.start()
                if not error and config.sudo:
                    assert supervisor is not None
                    pid_error = _wait_for_wprof_pid(
                        supervisor, pump, config.wprof_ready_timeout_s
                    )
                    if pid_error:
                        error = pid_error
                    else:
                        assert pump.profiler_pid is not None
                        profiler, pin_error = _pin_process(
                            pump.profiler_pid, privileged=True
                        )
                        if pin_error:
                            error = pin_error
                        else:
                            gate_error = _release_wprof_gate(
                                exec_gate, config.wprof_ready_timeout_s
                            )
                            if gate_error:
                                error = gate_error
                elif not error:
                    profiler = supervisor

                if not error:
                    assert supervisor is not None
                    ready_error = _wait_for_wprof_ready(
                        supervisor, pump, config.wprof_ready_timeout_s
                    )
                    if ready_error:
                        error = ready_error
                if not error:
                    assert profiler is not None
                    workload_done = threading.Event()
                    launch_bridge = _GuestLaunchBridge(
                        request, config.control_ack_timeout_s
                    )
                    controller = _WprofStopController(
                        config,
                        profiler,
                        request.guest_launch,
                        window.end_offset_s,
                        workload_done,
                    )
                    launch_bridge.start()
                    controller.start()
                    result = run_trial(request)
            except BaseException as exc:
                error = "; ".join(
                    part for part in (error, f"wprof trial failed: {exc}") if part
                )
                fatal = exc
            finally:
                if workload_done is not None:
                    workload_done.set()
                launch = (
                    launch_bridge.finish()
                    if launch_bridge is not None
                    else _GuestLaunchOutcome()
                )
                stop = controller.finish() if controller is not None else _WprofStopOutcome()
                if launch.error and launch.error not in error:
                    error = "; ".join(part for part in (error, launch.error) if part)
                if controller is not None:
                    if stop.error:
                        error = "; ".join(part for part in (error, stop.error) if part)
                    elif not stop.sent and fatal is None:
                        elapsed = (
                            0.0
                            if controller.launch is None
                            else time.monotonic() - controller.launch.monotonic_s
                        )
                        error = "; ".join(
                            part
                            for part in (
                                error,
                                f"step ended at {elapsed:.6f}s before the wprof window ended at "
                                f"{window.end_offset_s:.6f}s",
                            )
                            if part
                        )
                if process is not None:
                    teardown_error = _teardown_wprof_group(
                        process,
                        supervisor,
                        config,
                        graceful_interrupt_sent=stop.sent,
                    )
                    if teardown_error:
                        error = "; ".join(
                            part for part in (error, teardown_error) if part
                        )
                    profiler_returncode = process.returncode
                if pump is not None:
                    pump.join(config.profiler_exit_grace_s)
                    if pump.error:
                        error = "; ".join(part for part in (error, pump.error) if part)
    except BaseException as exc:
        error = "; ".join(part for part in (error, f"wprof trial failed: {exc}") if part)
        if process is not None and process.returncode is None:
            teardown_error = _teardown_wprof_group(
                process,
                supervisor,
                config,
                graceful_interrupt_sent=False,
            )
            if teardown_error:
                error = "; ".join(part for part in (error, teardown_error) if part)
            profiler_returncode = process.returncode
        fatal = exc
    finally:
        if profiler is not None and profiler is not supervisor:
            profiler.close()
        if supervisor is not None:
            supervisor.close()
    security_error = _secure_artifacts(
        (data_path, trace_path, log_path), config.sudo, config.profiler_exit_grace_s
    )
    if security_error:
        error = "; ".join(part for part in (error, security_error) if part)
    artifacts = tuple(
        artifact
        for artifact in (
            _artifact(capture_root, data_path, "wprof-data"),
            _artifact(capture_root, trace_path, "perfetto-trace"),
            _artifact(capture_root, log_path, "wprof-log"),
        )
        if artifact is not None
    )
    if fatal is not None and not isinstance(fatal, Exception):
        raise fatal
    required_sizes = {
        artifact.role: artifact.size_bytes
        for artifact in artifacts
        if artifact.role in {"wprof-data", "perfetto-trace"}
    }
    if required_sizes.get("wprof-data", 0) == 0 or required_sizes.get("perfetto-trace", 0) == 0:
        error = "; ".join(part for part in (error, "wprof produced incomplete artifacts") if part)
    if profiler_returncode not in (None, 0):
        error = "; ".join(
            part for part in (error, f"wprof exited {profiler_returncode}") if part
        )
    if result is not None and not result.ok:
        detail = f": {result.detail}" if result.detail else ""
        status = "with unavailable status" if result.returncode is None else f"{result.returncode}"
        error = "; ".join(
            part
            for part in (error, f"instrumented step exited {status}{detail}")
            if part
        )
    complete = (
        result is not None
        and result.ok
        and profiler_returncode == 0
        and not error
    )
    return CaptureTrialRecord(
        trial_id=trial_id,
        kind=CaptureKind.WPROF,
        state=CaptureState.COMPLETE if complete else CaptureState.FAILED,
        inner_jobs=selection.inner_jobs,
        expected_wall_s=selection.expected_wall_s,
        window=window,
        started_at=started_at,
        finished_at=_utc_now(),
        measured_wall_s=None if result is None else result.wall_s,
        workload_returncode=None if result is None else result.returncode,
        profiler_returncode=profiler_returncode,
        argv_prefix=(),
        artifacts=artifacts,
        error=error,
    )


def _manifest(
    *,
    path: Path,
    capture_id: str,
    state: CaptureState,
    created_at: str,
    finished_at: str,
    selection: SweetSpotSelection,
    preflight: Mapping[CaptureKind, ToolPreflight],
    trials: Sequence[CaptureTrialRecord],
    errors: Sequence[str],
    machine_id: str,
    container_class: str,
) -> CaptureManifest:
    return CaptureManifest(
        path=path,
        capture_id=capture_id,
        state=state,
        created_at=created_at,
        finished_at=finished_at,
        selection=selection,
        preflight=tuple(preflight[kind] for kind in (CaptureKind.PERF, CaptureKind.WPROF) if kind in preflight),
        trials=tuple(trials),
        errors=tuple(errors),
        machine_id=machine_id,
        container_class=container_class,
    )


def capture_at_sweet_spot(
    selection: SweetSpotSelection,
    config: CaptureConfig,
    run_trial: RunIsolatedTrial,
) -> CaptureManifest:
    """Capture requested profilers at the recommended width and retain a private manifest.

    Each wprof window gets a fresh isolated step execution.  Perf likewise gets its own execution,
    starts disabled, and records only after both ``enable`` and ``disable`` have been acknowledged
    over private FIFOs.  A failure raises :class:`ProfileCaptureError` *after* the failed manifest
    and any diagnostic artifacts have been written.
    """

    if not config.requested_kinds:
        raise ValueError("at least one profiler capture must be requested")
    config.output_dir.mkdir(parents=True, exist_ok=True)
    capture_id, capture_root = _new_capture_dir(config, selection)
    manifest_path = capture_root / "manifest.json"
    machine_id = current_machine_id()
    container_class = current_container_class()
    created_at = _utc_now()
    trials: list[CaptureTrialRecord] = []
    errors: list[str] = []
    preflight: dict[CaptureKind, ToolPreflight] = {}

    initial = _manifest(
        path=manifest_path,
        capture_id=capture_id,
        state=CaptureState.RUNNING,
        created_at=created_at,
        finished_at="",
        selection=selection,
        preflight=preflight,
        trials=trials,
        errors=errors,
        machine_id=machine_id,
        container_class=container_class,
    )
    _write_manifest(initial)
    try:
        preflight = preflight_capture_tools(config)
        for kind in config.requested_kinds:
            result = preflight[kind]
            if not result.usable:
                detail = result.diagnostic or f"{kind.value} preflight failed"
                errors.append(f"{kind.value}: {detail}")
        _write_manifest(
            _manifest(
                path=manifest_path,
                capture_id=capture_id,
                state=CaptureState.RUNNING,
                created_at=created_at,
                finished_at="",
                selection=selection,
                preflight=preflight,
                trials=trials,
                errors=errors,
                machine_id=machine_id,
                container_class=container_class,
            )
        )

        perf_preflight = preflight.get(CaptureKind.PERF)
        if config.capture_perf and perf_preflight is not None and perf_preflight.usable:
            record = _perf_trial(capture_root, selection, config, perf_preflight, run_trial)
            trials.append(record)
            if record.state is CaptureState.FAILED:
                errors.append(f"{record.trial_id}: {record.error}")
            _write_manifest(
                _manifest(
                    path=manifest_path,
                    capture_id=capture_id,
                    state=CaptureState.RUNNING,
                    created_at=created_at,
                    finished_at="",
                    selection=selection,
                    preflight=preflight,
                    trials=trials,
                    errors=errors,
                    machine_id=machine_id,
                    container_class=container_class,
                )
            )

        wprof_preflight = preflight.get(CaptureKind.WPROF)
        if config.wprof_windows and wprof_preflight is not None and wprof_preflight.usable:
            for index in range(1, config.wprof_windows + 1):
                record = _wprof_trial(
                    capture_root, index, selection, config, wprof_preflight, run_trial
                )
                trials.append(record)
                if record.state is CaptureState.FAILED:
                    errors.append(f"{record.trial_id}: {record.error}")
                    break
                _write_manifest(
                    _manifest(
                        path=manifest_path,
                        capture_id=capture_id,
                        state=CaptureState.RUNNING,
                        created_at=created_at,
                        finished_at="",
                        selection=selection,
                        preflight=preflight,
                        trials=trials,
                        errors=errors,
                        machine_id=machine_id,
                        container_class=container_class,
                    )
                )
    except BaseException as exc:
        errors.append(f"capture orchestration failed: {exc}")
        failed = _manifest(
            path=manifest_path,
            capture_id=capture_id,
            state=CaptureState.FAILED,
            created_at=created_at,
            finished_at=_utc_now(),
            selection=selection,
            preflight=preflight,
            trials=trials,
            errors=errors,
            machine_id=machine_id,
            container_class=container_class,
        )
        _write_manifest(failed)
        if isinstance(exc, (KeyboardInterrupt, SystemExit)):
            raise
        raise ProfileCaptureError(str(exc), failed) from exc

    state = CaptureState.COMPLETE if not errors else CaptureState.FAILED
    final = _manifest(
        path=manifest_path,
        capture_id=capture_id,
        state=state,
        created_at=created_at,
        finished_at=_utc_now(),
        selection=selection,
        preflight=preflight,
        trials=trials,
        errors=errors,
        machine_id=machine_id,
        container_class=container_class,
    )
    _write_manifest(final)
    if errors:
        raise ProfileCaptureError(errors[0], final)
    return final

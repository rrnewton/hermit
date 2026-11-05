"""Focused contracts for expensive post-sweep profiler captures."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import threading
import time
from pathlib import Path

import pytest

import dagrun.profile_capture as capture
from dagrun.estimates import SpeedupLevel, StepSpeedup
from dagrun.profile_capture import (
    CaptureConfig,
    CaptureKind,
    CaptureState,
    IsolatedTrialRequest,
    IsolatedTrialResult,
    ProfileCaptureError,
    SweetSpotSelection,
    ToolPreflight,
    capture_at_sweet_spot,
    centered_window,
    preflight_capture_tools,
    select_sweet_spot,
)


def _selection(expected_wall_s: float = 0.12) -> SweetSpotSelection:
    return SweetSpotSelection(
        step="build.app",
        workload_digest="0123456789abcdef",
        inner_jobs=8,
        expected_wall_s=expected_wall_s,
        baseline_inner_jobs=1,
        speedup=7.2,
        model_wall_s=expected_wall_s,
        raw_wall_s=expected_wall_s,
        git_sha="deadbeef",
    )


def _usable(kind: CaptureKind, binary: str) -> ToolPreflight:
    return ToolPreflight(
        kind=kind,
        requested_binary=binary,
        resolved_binary=binary,
        usable=True,
        returncode=0,
        version="test",
        diagnostic="",
        sudo=(),
    )


def test_select_sweet_spot_uses_recommended_level_and_raw_wall() -> None:
    levels = (
        SpeedupLevel(
            inner_jobs=1,
            samples=3,
            wall_s=10.0,
            raw_wall_s=12.0,
            wall_min_s=9.8,
            wall_max_s=10.2,
            cpu_s=10.0,
            effective_cores=1.0,
            throttled_s=0.0,
            speedup=1.0,
        ),
        SpeedupLevel(
            inner_jobs=8,
            samples=3,
            wall_s=1.4,
            raw_wall_s=1.6,
            wall_min_s=1.3,
            wall_max_s=1.5,
            cpu_s=10.8,
            effective_cores=7.1,
            throttled_s=0.0,
            speedup=7.142857,
        ),
    )
    model = StepSpeedup(
        step="build.app",
        baseline_inner_jobs=1,
        recommended_inner_jobs=8,
        measured_effective_cores=7.1,
        regression_inner_jobs=None,
        levels=levels,
    )

    selected = select_sweet_spot(model, workload_digest="digest", git_sha="abc")

    assert selected.inner_jobs == 8
    assert selected.expected_wall_s == 1.6
    assert selected.model_wall_s == 1.4
    assert selected.speedup == pytest.approx(7.142857)
    assert selected.source == "scaling-model-economic-plateau"


def test_centered_window_preserves_edges_and_reports_clipping() -> None:
    ordinary = centered_window(60.0, 0.4)
    assert ordinary.start_offset_s == pytest.approx(29.8)
    assert ordinary.end_offset_s == pytest.approx(30.2)
    assert not ordinary.clipped

    short = centered_window(0.25, 0.4)
    assert short.start_offset_s == pytest.approx(0.025)
    assert short.duration_s == pytest.approx(0.2)
    assert short.clipped


def test_perf_capture_uses_acknowledged_fifo_window_and_never_models_trial(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.PERF: _usable(CaptureKind.PERF, "/fake/perf")},
    )
    monkeypatch.setattr(capture, "current_machine_id", lambda: "test-machine")
    monkeypatch.setattr(capture, "current_container_class", lambda: "test-container")
    commands: list[tuple[str, float]] = []
    requests: list[IsolatedTrialRequest] = []
    launch_times: list[float] = []
    control_timeout_s = 10.0

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        requests.append(request)
        assert request.include_in_model is False
        assert request.inner_jobs == 8
        prefix = list(request.argv_prefix)
        control_spec = prefix[prefix.index("--control") + 1]
        assert control_spec.startswith("fifo:")
        control_name, ack_name = control_spec.removeprefix("fifo:").split(",", 1)
        output = Path(prefix[prefix.index("--output") + 1])
        responder_errors: list[BaseException] = []
        responder_done = threading.Event()

        def respond() -> None:
            try:
                with Path(ack_name).open("wb", buffering=0) as ack:
                    with Path(control_name).open("rb", buffering=0) as control:
                        for _ in range(2):
                            command = control.readline().decode("ascii").strip()
                            commands.append((command, time.monotonic()))
                            ack.write(b"ack\n")
                output.write_bytes(b"PERFILE2\0test")
            except BaseException as exc:
                responder_errors.append(exc)
            finally:
                responder_done.set()

        responder = threading.Thread(target=respond)
        responder.start()
        # Scheduler/cgroup/profiler setup is outside the guest clock.  The callback reports the
        # exact inner-command boundary only after that deliberately long setup delay.
        time.sleep(0.08)
        launch_times.append(time.monotonic())
        request.notify_guest_launched(request.step, os.getpid(), launch_times[-1])
        # Model a guest that remains alive through the requested capture window.  Waiting for
        # the acknowledged disable is the protocol boundary; a fixed sleep plus a shorter join
        # raced the controller when the full validation DAG saturated the host.
        responder_finished = responder_done.wait(timeout=control_timeout_s)
        assert responder_finished, "perf FIFO responder did not finish"
        responder.join()
        assert not responder_errors
        return IsolatedTrialResult(returncode=0, wall_s=0.14)

    manifest = capture_at_sweet_spot(
        _selection(),
        CaptureConfig(
            output_dir=tmp_path,
            capture_perf=True,
            perf_window_s=0.02,
            control_ack_timeout_s=control_timeout_s,
        ),
        run_trial,
    )

    assert [command for command, _when in commands] == ["enable", "disable"]
    assert commands[0][1] >= launch_times[0] + 0.045
    assert commands[1][1] >= launch_times[0] + 0.065
    assert len(requests) == 1
    assert manifest.state is CaptureState.COMPLETE
    assert manifest.trials[0].included_in_model is False
    assert manifest.trials[0].state is CaptureState.COMPLETE
    assert manifest.trials[0].workload_returncode is None
    assert manifest.trials[0].profiler_returncode == 0
    assert manifest.machine_id == "test-machine"
    assert manifest.container_class == "test-container"
    assert stat.S_IMODE(manifest.path.parent.stat().st_mode) == 0o700
    assert stat.S_IMODE(manifest.path.stat().st_mode) == 0o600
    artifact = manifest.path.parent / manifest.trials[0].artifacts[0].path
    assert stat.S_IMODE(artifact.stat().st_mode) == 0o600
    payload = json.loads(manifest.path.read_text(encoding="utf-8"))
    assert payload["schema"] == "dagrun-profile-capture-v1"
    assert payload["machine_id"] == "test-machine"
    assert payload["container_class"] == "test-container"
    assert payload["trials"][0]["included_in_model"] is False


def _fake_wprof(
    path: Path,
    exit_code: int = 0,
    *,
    ready_delay_s: float = 0.02,
    announce_ready: bool = True,
    early_exit_after_ready_s: float | None = None,
    spawn_descendant: bool = False,
) -> None:
    path.write_text(
        """#!/usr/bin/env python3
import pathlib
import os
import signal
import sys
import time

def value(prefix):
    return next(arg.split('=', 1)[1] for arg in sys.argv[1:] if arg.startswith(prefix))

stopped = False
def stop(signum, frame):
    global stopped
    stopped = True

signal.signal(signal.SIGINT, stop)
print('ARGS ' + ' '.join(sys.argv[1:]), flush=True)
if SPAWN_DESCENDANT:
    child = os.fork()
    if child == 0:
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        os.close(1)
        os.close(2)
        while True:
            time.sleep(1)
    pathlib.Path('descendant.pid').write_text(str(child))
time.sleep(READY_DELAY)
if ANNOUNCE_READY:
    print('Running in flight recorder mode, press Ctrl-C to stop...', flush=True)
if EARLY_EXIT is not None:
    time.sleep(EARLY_EXIT)
    raise SystemExit(EXIT_CODE)
while not stopped:
    time.sleep(0.002)
pathlib.Path(value('--data=')).write_bytes(b'wprof-data')
pathlib.Path(value('--trace=')).write_bytes(b'perfetto-trace')
print('fake wprof complete', flush=True)
raise SystemExit(EXIT_CODE)
"""
        .replace("EXIT_CODE", str(exit_code))
        .replace("READY_DELAY", repr(ready_delay_s))
        .replace("ANNOUNCE_READY", repr(announce_ready))
        .replace("EARLY_EXIT", repr(early_exit_after_ready_s))
        .replace("SPAWN_DESCENDANT", repr(spawn_descendant)),
        encoding="utf-8",
    )
    path.chmod(0o755)


def test_each_wprof_window_is_a_private_separate_recommended_width_trial(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-wprof"
    _fake_wprof(fake)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )
    requests: list[IsolatedTrialRequest] = []
    successful_interrupts: list[float] = []
    interrupt_observed = threading.Condition()
    protocol_timeout_s = 10.0
    send_signal = capture._send_pinned_wprof_signal

    def observe_signal(
        config: CaptureConfig,
        identity: capture._PinnedProcess,
        signal_name: str,
        timeout_s: float,
    ) -> str:
        sent_at = time.monotonic()
        error = send_signal(config, identity, signal_name, timeout_s)
        if signal_name == "INT" and not error:
            with interrupt_observed:
                successful_interrupts.append(sent_at)
                interrupt_observed.notify_all()
        return error

    monkeypatch.setattr(capture, "_send_pinned_wprof_signal", observe_signal)

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        requests.append(request)
        with interrupt_observed:
            interrupts_before = len(successful_interrupts)
        assert interrupts_before == len(requests) - 1
        assert request.argv_prefix == ()
        assert request.include_in_model is False
        # The callback cannot begin until the readiness line has been persisted.
        log = request.output_dir / "wprof.log"
        assert "Running in flight recorder mode" in log.read_text(encoding="utf-8")
        time.sleep(0.08)
        launched_at = time.monotonic()
        request.notify_guest_launched(request.step, os.getpid(), launched_at)
        # The controller stops wprof at the selected window end while this workload continues.
        # Wait for that protocol event rather than assuming the controller thread will be
        # scheduled within a fixed 90 ms while the validation DAG is saturating the host.
        with interrupt_observed:
            interrupted = interrupt_observed.wait_for(
                lambda: len(successful_interrupts) == interrupts_before + 1,
                timeout=protocol_timeout_s,
            )
            assert interrupted, "wprof controller did not send its window-ending interrupt"
            interrupted_at = successful_interrupts[interrupts_before]
        assert request.window is not None
        assert interrupted_at >= launched_at + request.window.end_offset_s
        return IsolatedTrialResult(returncode=0, wall_s=0.13)

    manifest = capture_at_sweet_spot(
        _selection(),
        CaptureConfig(output_dir=tmp_path, wprof_windows=2, wprof_window_s=0.02),
        run_trial,
    )

    assert manifest.state is CaptureState.COMPLETE
    assert len(successful_interrupts) == len(requests) == 2
    assert [request.trial_id for request in requests] == ["wprof-001", "wprof-002"]
    assert all(request.inner_jobs == 8 for request in requests)
    assert [trial.state for trial in manifest.trials] == [
        CaptureState.COMPLETE,
        CaptureState.COMPLETE,
    ]
    for trial in manifest.trials:
        assert {artifact.role for artifact in trial.artifacts} == {
            "wprof-data",
            "perfetto-trace",
            "wprof-log",
        }
        trial_dir = manifest.path.parent / trial.trial_id
        assert (trial_dir / "wprof.data").read_bytes() == b"wprof-data"
        assert (trial_dir / "trace.pb").read_bytes() == b"perfetto-trace"
        assert stat.S_IMODE(trial_dir.stat().st_mode) == 0o700
        for artifact in trial.artifacts:
            path = manifest.path.parent / artifact.path
            assert path.stat().st_size > 0
            assert stat.S_IMODE(path.stat().st_mode) == 0o600
        log_text = (trial_dir / "wprof.log").read_text(encoding="utf-8")
        assert "--flight-record=0.020000s" in log_text
        assert "--activate=" not in log_text
        assert "--dur=" not in log_text


def test_wprof_failure_is_explicit_and_retains_manifest_and_log(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "failing-wprof"
    _fake_wprof(fake, exit_code=7)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        request.notify_guest_launched(request.step, os.getpid(), time.monotonic())
        time.sleep(0.13)
        return IsolatedTrialResult(returncode=0, wall_s=0.13)

    with pytest.raises(ProfileCaptureError, match="capture manifest") as raised:
        capture_at_sweet_spot(
            _selection(),
            CaptureConfig(output_dir=tmp_path, wprof_windows=2, wprof_window_s=0.02),
            run_trial,
        )

    manifest = raised.value.manifest
    assert manifest.path.exists()
    assert manifest.state is CaptureState.FAILED
    assert len(manifest.trials) == 1
    assert manifest.trials[0].profiler_returncode == 7
    assert "wprof exited 7" in manifest.trials[0].error
    payload = json.loads(manifest.path.read_text(encoding="utf-8"))
    assert payload["state"] == "failed"
    assert payload["trials"][0]["state"] == "failed"
    log = next(
        artifact for artifact in manifest.trials[0].artifacts if artifact.role == "wprof-log"
    )
    assert (manifest.path.parent / log.path).read_text(encoding="utf-8").strip()


def test_wprof_readiness_timeout_never_launches_workload_and_is_retained(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "silent-wprof"
    _fake_wprof(fake, announce_ready=False)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )
    launched = False

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        nonlocal launched
        launched = True
        return IsolatedTrialResult(returncode=0, wall_s=0.1)

    with pytest.raises(ProfileCaptureError) as raised:
        capture_at_sweet_spot(
            _selection(),
            CaptureConfig(
                output_dir=tmp_path,
                wprof_windows=1,
                wprof_window_s=0.02,
                wprof_ready_timeout_s=0.05,
                profiler_exit_grace_s=0.1,
            ),
            run_trial,
        )

    assert not launched
    assert raised.value.manifest.path.exists()
    assert "did not announce flight-recorder readiness" in raised.value.manifest.trials[0].error


def test_wprof_early_exit_after_ready_fails_via_pinned_identity_without_killing_workload(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "early-exit-wprof"
    _fake_wprof(fake, early_exit_after_ready_s=0.01)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )
    workload_finished = False

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        nonlocal workload_finished
        time.sleep(0.03)
        request.notify_guest_launched(request.step, os.getpid(), time.monotonic())
        time.sleep(0.12)
        workload_finished = True
        return IsolatedTrialResult(returncode=0, wall_s=0.12)

    with pytest.raises(ProfileCaptureError) as raised:
        capture_at_sweet_spot(
            _selection(),
            CaptureConfig(output_dir=tmp_path, wprof_windows=1, wprof_window_s=0.02),
            run_trial,
        )

    assert workload_finished
    trial = raised.value.manifest.trials[0]
    assert trial.state is CaptureState.FAILED
    assert "wprof exited" in trial.error
    assert "wprof produced incomplete artifacts" in trial.error


def test_wprof_teardown_kills_profiler_descendants(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "child-spawning-wprof"
    _fake_wprof(fake, spawn_descendant=True)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        request.notify_guest_launched(request.step, os.getpid(), time.monotonic())
        time.sleep(0.13)
        return IsolatedTrialResult(returncode=0, wall_s=0.13)

    manifest = capture_at_sweet_spot(
        _selection(),
        CaptureConfig(output_dir=tmp_path, wprof_windows=1, wprof_window_s=0.02),
        run_trial,
    )

    descendant_pid = int(
        (manifest.path.parent / "wprof-001" / "descendant.pid").read_text()
    )
    deadline = time.monotonic() + 1.0
    while Path(f"/proc/{descendant_pid}").exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert not Path(f"/proc/{descendant_pid}").exists()


def test_privileged_artifacts_are_reclaimed_by_exact_path_then_made_private(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    artifact = tmp_path / "trace.pb"
    artifact.write_bytes(b"trace")
    artifact.chmod(0o644)
    commands: list[list[str]] = []

    def fake_run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        del kwargs
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr("dagrun.profile_capture.subprocess.run", fake_run)

    error = capture._secure_artifacts((artifact,), ("sudo", "-n"), 1.0)

    assert error == ""
    assert commands == [
        [
            "sudo",
            "-n",
            "/bin/chown",
            "--no-dereference",
            f"{os.getuid()}:{os.getgid()}",
            "--",
            str(artifact),
        ]
    ]
    assert stat.S_IMODE(artifact.stat().st_mode) == 0o600


def test_privileged_wprof_command_reports_the_exec_pid_and_uses_flight_mode(
    tmp_path: Path,
) -> None:
    selection = _selection(expected_wall_s=1.0)
    window = centered_window(selection.expected_wall_s, 0.4)

    command = capture._wprof_flight_command(
        CaptureConfig(output_dir=tmp_path, wprof_windows=1, sudo=("sudo", "-n")),
        "/usr/bin/wprof",
        data_path=tmp_path / "wprof.data",
        trace_path=tmp_path / "trace.pb",
        window=window,
        selection=selection,
        launch_gate=tmp_path / "exec-gate.fifo",
    )

    assert command[:4] == ("sudo", "-n", "/bin/sh", "-c")
    assert "DAGRUN_WPROF_PID" in command[4]
    assert command[5:8] == (
        "dagrun-wprof",
        str(tmp_path / "exec-gate.fifo"),
        "/usr/bin/wprof",
    )
    assert "--flight-record=0.400000s" in command
    assert not any(argument.startswith("--activate=") for argument in command)
    assert not any(argument.startswith("--dur=") for argument in command)


def test_privileged_wprof_exec_gate_and_pidfd_signal_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    fake = tmp_path / "fake-privileged-wprof"
    _fake_wprof(fake)
    monkeypatch.setattr(
        capture,
        "preflight_capture_tools",
        lambda config: {CaptureKind.WPROF: _usable(CaptureKind.WPROF, str(fake))},
    )

    def run_trial(request: IsolatedTrialRequest) -> IsolatedTrialResult:
        request.notify_guest_launched(request.step, os.getpid(), time.monotonic())
        time.sleep(0.13)
        return IsolatedTrialResult(returncode=0, wall_s=0.13)

    # `env` exercises the explicit-privilege wrapper/gate/pidfd helper without requiring root.
    manifest = capture_at_sweet_spot(
        _selection(),
        CaptureConfig(
            output_dir=tmp_path,
            wprof_windows=1,
            wprof_window_s=0.02,
            sudo=("/usr/bin/env",),
        ),
        run_trial,
    )

    assert manifest.state is CaptureState.COMPLETE
    log = (manifest.path.parent / "wprof-001" / "wprof.log").read_text()
    assert log.index("DAGRUN_WPROF_PID=") < log.index("Running in flight recorder mode")


def test_preflight_reports_missing_tools_without_trying_sudo(tmp_path: Path) -> None:
    config = CaptureConfig(
        output_dir=tmp_path,
        capture_perf=True,
        wprof_windows=1,
        perf_binary="dagrun-test-no-such-perf",
        wprof_binary="dagrun-test-no-such-wprof",
    )

    results = preflight_capture_tools(config)

    assert not results[CaptureKind.PERF].usable
    assert not results[CaptureKind.WPROF].usable
    assert results[CaptureKind.PERF].sudo == ()
    assert results[CaptureKind.WPROF].sudo == ()
    assert not list(tmp_path.glob(".capture-preflight-*"))

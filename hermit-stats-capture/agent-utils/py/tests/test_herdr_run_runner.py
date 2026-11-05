"""Execution and result-collection tests.

The fake client runs the generated shell line through a REAL bash, so these are not string
assertions about what we think the shell would do — the quoting, redirection, and exit-code capture
are actually executed.
"""

from __future__ import annotations

import json
import os
import shlex
import threading
from typing import cast

import pytest

import herdr_run.state as state
from herdr_run.allowlist import admit
from herdr_run.client import HerdrClient
from herdr_run.config import MAX_TIMEOUT_SECONDS, Config
from herdr_run.errors import ConfigError, HerdrUnavailable, PaneBusy, RunTimeout
from herdr_run.reap import evidence_from_runs
from herdr_run.runner import (
    RunResult,
    build_shell_command,
    execute,
    make_run_id,
    read_output_bytes,
    spool_paths,
    wait_ready,
    write_meta,
)
from herdr_run.session import resolve_target
from tests.herdr_fake import FakeHerdrClient


@pytest.fixture(autouse=True)
def _isolated_account_state(tmp_path: object, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(state, "_account_home", lambda: os.path.join(str(tmp_path), "account-home"))


def _config(root: str, **kwargs: object) -> Config:
    base: dict[str, object] = {"project_root": root, "spool_dir": "spool", "allow": ("git", "echo", "false", "sleep", "cat")}
    base.update(kwargs)
    return Config(**base)  # type: ignore[arg-type]


def _client(fake: FakeHerdrClient) -> HerdrClient:
    return cast(HerdrClient, fake)


def _run(fake: FakeHerdrClient, config: Config, command: str, **kwargs: object) -> object:
    target = resolve_target(_client(fake), config, "agent")
    return execute(
        _client(fake),
        config,
        target,
        admit(command, config),
        agent="agent",
        cwd=config.project_root,
        ready_timeout=0.0,
        timeout=float(cast(float, kwargs.pop("timeout", 30.0))),
        poll_interval=0.01,
        home="/nonexistent",  # force the prompt signal to abstain; process signal decides
        **kwargs,  # type: ignore[arg-type]
    )


# --- result capture ------------------------------------------------------------------------------


def test_captures_stdout_and_zero_exit_code(tmp_path: object) -> None:
    root = str(tmp_path)
    result = _run(FakeHerdrClient(), _config(root), "echo hello-from-pane")
    assert result.exit_code == 0  # type: ignore[attr-defined]
    assert result.stdout.strip() == "hello-from-pane"  # type: ignore[attr-defined]


def test_captures_nonzero_exit_code(tmp_path: object) -> None:
    """A failing command is a RESULT, not a herdr-run error: the real exit code comes back."""
    result = _run(FakeHerdrClient(), _config(str(tmp_path)), "false")
    assert result.exit_code == 1  # type: ignore[attr-defined]


def test_captures_stderr_separately(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root, allow=("bash",))
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    result = execute(
        _client(fake),
        config,
        target,
        admit("bash -c 'echo OUT; echo ERR >&2'", config),
        agent="agent",
        cwd=root,
        ready_timeout=0.0,
        timeout=30.0,
        poll_interval=0.01,
        home="/nonexistent",
    )
    assert result.stdout.strip() == "OUT"
    assert result.stderr.strip() == "ERR"


def test_arguments_with_spaces_and_metacharacters_survive_intact(tmp_path: object) -> None:
    """End-to-end proof that re-quoting holds through a real shell.

    ``; rm -rf /`` must arrive as ONE literal argument, not as a second command.
    """
    root = str(tmp_path)
    config = _config(root, allow=("printf",))
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    result = execute(
        _client(fake),
        config,
        target,
        admit("printf '[%s]' 'two words' '; rm -rf /' '$(id)' '`id`'", config),
        agent="agent",
        cwd=root,
        ready_timeout=0.0,
        timeout=30.0,
        poll_interval=0.01,
        home="/nonexistent",
    )
    assert result.stdout == "[two words][; rm -rf /][$(id)][`id`]"


def test_runs_in_the_requested_directory(tmp_path: object) -> None:
    root = str(tmp_path)
    workdir = os.path.join(root, "sub dir with spaces")
    os.makedirs(workdir)
    config = _config(root, allow=("pwd",))
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    result = execute(
        _client(fake),
        config,
        target,
        admit("pwd", config),
        agent="agent",
        cwd=workdir,
        ready_timeout=0.0,
        timeout=30.0,
        poll_interval=0.01,
        home="/nonexistent",
    )
    assert result.stdout.strip() == workdir


def test_failed_cd_is_reported_as_a_failure(tmp_path: object) -> None:
    """A missing working directory must fail the run, not silently run somewhere else."""
    root = str(tmp_path)
    config = _config(root, allow=("pwd",))
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    result = execute(
        _client(fake),
        config,
        target,
        admit("pwd", config),
        agent="agent",
        cwd=os.path.join(root, "does-not-exist"),
        ready_timeout=0.0,
        timeout=30.0,
        poll_interval=0.01,
        home="/nonexistent",
    )
    assert result.exit_code != 0


def test_spool_files_are_written(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    result = cast(RunResult, _run(FakeHerdrClient(), config, "echo spooled"))
    spool = spool_paths(config, result.run_id)
    meta = write_meta(result, admit("echo spooled", config), config, "agent")
    for directory in (
        os.path.join(root, "spool"),
        os.path.join(root, "spool", "runs"),
        spool.directory,
    ):
        assert os.stat(directory).st_mode & 0o777 == 0o700
    for path in (
        spool.stdout,
        spool.stderr,
        spool.exit_code,
        os.path.join(spool.directory, "command"),
        meta,
    ):
        assert os.path.isfile(path), path
        assert os.stat(path).st_mode & 0o777 == 0o600


def test_binary_spool_bytes_are_preserved_without_text_decoding(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root, allow=("python3",))
    command = (
        "python3 -c 'import os;"
        "os.write(1,bytes([0,255,13,10]));"
        "os.write(2,bytes([254,66]))'"
    )
    result = cast(RunResult, _run(FakeHerdrClient(), config, command))

    stdout, stderr = read_output_bytes(result)
    assert stdout == bytes([0, 255, 13, 10])
    assert stderr == bytes([254, 66])
    with open(result.spool.stdout, "rb") as handle:
        assert handle.read() == stdout
    with open(result.spool.stderr, "rb") as handle:
        assert handle.read() == stderr


def test_existing_configured_spool_root_is_never_chmoded(tmp_path: object) -> None:
    root = str(tmp_path)
    os.chmod(root, 0o755)
    result = cast(
        RunResult,
        _run(FakeHerdrClient(), _config(root, spool_dir="."), "echo preserved"),
    )

    assert result.exit_code == 0
    assert os.stat(root).st_mode & 0o777 == 0o755
    assert os.stat(os.path.join(root, "runs")).st_mode & 0o777 == 0o700
    assert os.stat(result.spool.directory).st_mode & 0o777 == 0o700


# --- the shell wrapper ------------------------------------------------------------------------------


def test_shell_wrapper_quotes_every_interpolated_path() -> None:
    config = Config(allow=("git",))
    spool = spool_paths(Config(project_root="/tmp/root x", spool_dir="sp ool"), "run 1")
    line = build_shell_command(admit("git status", config), spool, "/tmp/work dir")
    # The interactive pane shell sees only a portable `command sh -c ONE_ARGUMENT` shape. The
    # potentially POSIX-specific grouping/redirection syntax lives inside that sh argument, so a
    # fish prompt can launch it without trying to parse `{ ...; }` itself.
    outer = shlex.split(line)
    assert outer[:3] == ["command", "sh", "-c"]
    assert len(outer) == 4
    inner = outer[3]
    assert shlex.quote("/tmp/work dir") in inner
    assert shlex.quote(spool.stdout) in inner
    assert shlex.quote(spool.stderr) in inner
    assert shlex.quote(spool.exit_code) in inner


def test_shell_wrapper_refuses_terminal_controls_in_paths() -> None:
    config = Config(allow=("git",))
    spool = spool_paths(Config(project_root="/tmp", spool_dir="spool"), "run")
    with pytest.raises(HerdrUnavailable, match="terminal control"):
        build_shell_command(admit("git status", config), spool, "/tmp/work\x1b[2J")


def test_same_second_runs_allocate_distinct_spools(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    fixed_now = lambda: 1_700_000_000.0

    first = execute(
        _client(fake),
        config,
        target,
        admit("echo first", config),
        agent="agent",
        cwd=root,
        ready_timeout=0.0,
        timeout=30.0,
        now=fixed_now,
        poll_interval=0.0,
        home="/nonexistent",
    )
    second = execute(
        _client(fake),
        config,
        target,
        admit("false", config),
        agent="agent",
        cwd=root,
        ready_timeout=0.0,
        timeout=30.0,
        now=fixed_now,
        poll_interval=0.0,
        home="/nonexistent",
    )

    assert first.run_id != second.run_id
    assert first.spool.directory != second.spool.directory
    assert first.exit_code == 0
    assert second.exit_code == 1
    assert os.path.isdir(first.spool.directory)
    assert os.path.isdir(second.spool.directory)


def test_preplanted_exit_code_cannot_complete_a_new_run(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient(execute_locally=False)
    target = resolve_target(_client(fake), config, "agent")
    fixed_epoch = 1_700_000_000.0
    base_id = make_run_id("agent", now=fixed_epoch, pid=os.getpid())
    planted = spool_paths(config, base_id)
    os.makedirs(planted.directory)
    with open(planted.exit_code, "w", encoding="utf-8") as handle:
        handle.write("91\n")
    with open(planted.stdout, "w", encoding="utf-8") as handle:
        handle.write("stale output\n")

    with pytest.raises(RunTimeout):
        execute(
            _client(fake),
            config,
            target,
            admit("echo never-ran", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=0.0,
            now=lambda: fixed_epoch,
            poll_interval=0.0,
            home="/nonexistent",
        )

    fresh = spool_paths(config, f"{base_id}-1")
    assert os.path.isdir(fresh.directory)
    assert not os.path.exists(fresh.exit_code)
    with open(planted.exit_code, encoding="utf-8") as handle:
        assert handle.read() == "91\n"
    assert len(fake.commands) == 1


# --- readiness gating ---------------------------------------------------------------------------------


def test_refuses_to_run_in_a_busy_pane(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    fake.busy_panes.add(target.pane_id)

    with pytest.raises(PaneBusy, match="not ready"):
        execute(
            _client(fake),
            config,
            target,
            admit("echo nope", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=5.0,
            poll_interval=0.01,
            home="/nonexistent",
        )
    # The decisive assertion: NOTHING was typed into the pane.
    assert fake.commands == []


def test_refuses_when_the_prompt_line_is_dirty(tmp_path: object) -> None:
    """Process signal says idle; the prompt veto must still stop us."""
    root = str(tmp_path)
    config = _config(root, prompt_tail="$ ")
    fake = FakeHerdrClient(pane_text="[user@host ~]\n$ half-typed-by-a-human\n")
    target = resolve_target(_client(fake), config, "agent")

    with pytest.raises(PaneBusy, match="not ready"):
        execute(
            _client(fake),
            config,
            target,
            admit("echo nope", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=5.0,
            poll_interval=0.01,
        )
    assert fake.commands == []


def test_readiness_process_mode_ignores_the_prompt_veto(tmp_path: object) -> None:
    """Positive half of the previous test: the veto is configurable, and dropping it lets it run."""
    root = str(tmp_path)
    config = _config(root, prompt_tail="$ ", readiness="process")
    fake = FakeHerdrClient(pane_text="[user@host ~]\n$ half-typed-by-a-human\n")
    result = _run(fake, config, "echo ran-anyway")
    assert result.exit_code == 0  # type: ignore[attr-defined]


def test_default_zero_ready_timeout_still_confirms_twice(tmp_path: object) -> None:
    """Regression: with ready_timeout 0 the two-consecutive-reading rule must still be satisfiable.

    An earlier version broke out of the poll loop on the deadline before a second reading could be
    taken, so an idle pane could never be declared ready under the default config.
    """
    fake = FakeHerdrClient()
    config = _config(str(tmp_path))
    target = resolve_target(_client(fake), config, "agent")
    readiness = wait_ready(
        _client(fake), config, target, prompt_tail=None, timeout=0.0, poll_interval=0.0
    )
    assert readiness.ready is True


@pytest.mark.parametrize("timeout", [float("nan"), float("inf"), -1.0, MAX_TIMEOUT_SECONDS + 1])
def test_public_readiness_api_rejects_invalid_timeouts(
    tmp_path: object, timeout: float
) -> None:
    config = _config(str(tmp_path))
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")

    with pytest.raises(ConfigError):
        wait_ready(_client(fake), config, target, prompt_tail=None, timeout=timeout)


@pytest.mark.parametrize("which", ["ready", "command"])
@pytest.mark.parametrize("timeout", [float("nan"), float("inf"), -1.0, MAX_TIMEOUT_SECONDS + 1])
def test_public_execute_api_rejects_invalid_timeouts_before_launch(
    tmp_path: object, which: str, timeout: float
) -> None:
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    ready_timeout = timeout if which == "ready" else 0.0
    command_timeout = timeout if which == "command" else 1.0

    with pytest.raises(ConfigError):
        execute(
            _client(fake),
            config,
            target,
            admit("git status", config),
            agent="agent",
            cwd=root,
            home="/nonexistent",
            ready_timeout=ready_timeout,
            timeout=command_timeout,
        )
    assert fake.commands == []


def test_waits_for_a_pane_that_becomes_free(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient()
    target = resolve_target(_client(fake), config, "agent")
    fake.busy_panes.add(target.pane_id)

    calls = {"n": 0}

    def fake_sleep(_seconds: float) -> None:
        calls["n"] += 1
        if calls["n"] >= 3:
            fake.busy_panes.discard(target.pane_id)

    readiness = wait_ready(
        _client(fake), config, target, prompt_tail=None, timeout=10.0, sleep=fake_sleep, poll_interval=0.0
    )
    assert readiness.ready is True


def test_per_pane_lock_prevents_two_concurrent_launches(tmp_path: object) -> None:
    """Different project spools still share the account-global lock for the same pane."""

    class BlockingFirstRun(FakeHerdrClient):
        def __init__(self) -> None:
            super().__init__()
            self.entered = threading.Event()
            self.release = threading.Event()
            self._count_lock = threading.Lock()
            self._run_count = 0

        def run(self, pane_id: str, command: str) -> None:
            with self._count_lock:
                self._run_count += 1
                ordinal = self._run_count
            if ordinal == 1:
                self.entered.set()
                if not self.release.wait(timeout=5.0):
                    raise AssertionError("test did not release the first pane run")
            super().run(pane_id, command)

    root = str(tmp_path)
    first_root = os.path.join(root, "project-one")
    second_root = os.path.join(root, "project-two")
    os.makedirs(first_root)
    os.makedirs(second_root)
    first_config = _config(first_root)
    second_config = _config(second_root)
    fake = BlockingFirstRun()
    target = resolve_target(_client(fake), first_config, "agent")
    first_results: list[RunResult] = []
    first_errors: list[BaseException] = []

    def launch_first() -> None:
        try:
            first_results.append(
                execute(
                    _client(fake),
                    first_config,
                    target,
                    admit("echo first", first_config),
                    agent="agent",
                    cwd=first_root,
                    ready_timeout=0.0,
                    timeout=30.0,
                    poll_interval=0.0,
                    home="/nonexistent",
                )
            )
        except BaseException as exc:  # preserve the worker failure for the main assertion
            first_errors.append(exc)

    worker = threading.Thread(target=launch_first, daemon=True)
    worker.start()
    assert fake.entered.wait(timeout=5.0), "first run never reached the pane launch"
    try:
        with pytest.raises(PaneBusy, match="already reserved"):
            execute(
                _client(fake),
                second_config,
                target,
                admit("echo second", second_config),
                agent="agent",
                cwd=second_root,
                ready_timeout=0.0,
                timeout=30.0,
                poll_interval=0.0,
                home="/nonexistent",
            )
    finally:
        fake.release.set()
        worker.join(timeout=5.0)

    assert not worker.is_alive()
    assert first_errors == []
    assert [result.stdout.strip() for result in first_results] == ["first"]
    assert len(fake.commands) == 1


# --- timeout -------------------------------------------------------------------------------------------


def test_timeout_reports_that_the_command_is_still_running(tmp_path: object) -> None:
    root = str(tmp_path)
    config = _config(root)
    # execute_locally=False: the line is never run, so the exit-code file never appears.
    fake = FakeHerdrClient(execute_locally=False)
    target = resolve_target(_client(fake), config, "agent")

    with pytest.raises(RunTimeout, match="STILL RUNNING"):
        execute(
            _client(fake),
            config,
            target,
            admit("echo never-completes", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=0.05,
            poll_interval=0.01,
            home="/nonexistent",
        )
    # It WAS launched — the timeout is about collection, not about admission.
    assert len(fake.commands) == 1


def test_a_timed_out_run_is_recorded_as_unfinished(tmp_path: object) -> None:
    """The ONLY writer of the state the reaper calls IN FLIGHT.

    A command that outlived its deadline is still running in a pane nobody owns. If the timeout
    left no record, the one pane that provably still has work in it would be the one pane the
    reaper had no evidence about, and ``exit_code: null`` would be a state no writer could produce.
    """
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient(execute_locally=False)
    target = resolve_target(_client(fake), config, "agent")

    with pytest.raises(RunTimeout):
        execute(
            _client(fake),
            config,
            target,
            admit("echo never-completes", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=0.05,
            poll_interval=0.01,
            home="/nonexistent",
        )

    runs = os.path.join(root, "spool", "runs")
    run_id = os.listdir(runs)[0]
    with open(os.path.join(runs, run_id, "meta.json"), encoding="utf-8") as handle:
        document = json.load(handle)
    assert document["exit_code"] is None, "a run still running must not record a status"
    assert document["pane_id"] == target.pane_id
    flags, recorded = evidence_from_runs(target.pane_id, [document])
    assert flags == (False,), "the reaper must read this as work in flight"
    assert recorded is not None


def test_partially_written_exit_code_is_not_read_as_a_result(tmp_path: object) -> None:
    """A non-integer exit-code file means "still being written", never a corrupt result."""
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient(execute_locally=False)
    target = resolve_target(_client(fake), config, "agent")

    original_run = fake.run

    def run_and_write_garbage(pane_id: str, command: str) -> None:
        original_run(pane_id, command)
        run_id = os.listdir(os.path.join(root, "spool", "runs"))[0]
        with open(os.path.join(root, "spool", "runs", run_id, "exit_code"), "w", encoding="utf-8") as handle:
            handle.write("")  # created but empty, as during a torn write

    fake.run = run_and_write_garbage  # type: ignore[method-assign]

    with pytest.raises(RunTimeout):
        execute(
            _client(fake),
            config,
            target,
            admit("echo x", config),
            agent="agent",
            cwd=root,
            ready_timeout=0.0,
            timeout=0.05,
            poll_interval=0.01,
            home="/nonexistent",
        )


# --- the timeout path must not swallow what the command already printed --------------------------


def test_timeout_carries_the_partial_output(tmp_path: object) -> None:
    """A timed-out run that printed something must not look like one that printed nothing."""
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient(execute_locally=False)
    target = resolve_target(_client(fake), config, "agent")

    original_run = fake.run

    def run_and_emit_partial(pane_id: str, command: str) -> None:
        original_run(pane_id, command)
        run_id = os.listdir(os.path.join(root, "spool", "runs"))[0]
        base = os.path.join(root, "spool", "runs", run_id)
        with open(os.path.join(base, "stdout"), "w", encoding="utf-8") as handle:
            handle.write("PARTIAL-STDOUT\n")
        with open(os.path.join(base, "stderr"), "w", encoding="utf-8") as handle:
            handle.write("PARTIAL-STDERR\n")

    fake.run = run_and_emit_partial  # type: ignore[method-assign]

    with pytest.raises(RunTimeout) as excinfo:
        execute(
            _client(fake), config, target, admit("echo x", config), agent="agent", cwd=root,
            ready_timeout=0.0, timeout=0.05, poll_interval=0.01, home="/nonexistent",
        )
    assert excinfo.value.partial_stdout == "PARTIAL-STDOUT\n"
    assert excinfo.value.partial_stderr == "PARTIAL-STDERR\n"


def test_retention_runs_on_write(tmp_path: object) -> None:
    """Prune-on-write: an old run directory is gone after the next run, with no timer involved."""
    import time as _time

    root = str(tmp_path)
    config = _config(root)
    runs = os.path.join(root, "spool", "runs")
    os.makedirs(runs)
    stale = os.path.join(runs, "20260101T000000-old-1")
    os.makedirs(stale)
    with open(os.path.join(stale, "exit_code"), "w", encoding="utf-8") as handle:
        handle.write("0\n")
    old = _time.time() - 9 * 86400
    os.utime(os.path.join(stale, "exit_code"), (old, old))
    os.utime(stale, (old, old))

    result = _run(FakeHerdrClient(), config, "echo fresh")

    assert result.exit_code == 0  # type: ignore[attr-defined]
    assert not os.path.exists(stale), "writing a new run must prune the stale one"
    assert os.path.isdir(result.spool.directory)  # type: ignore[attr-defined]


def test_zero_day_retention_does_not_delete_the_run_being_created(tmp_path: object) -> None:
    root = str(tmp_path)
    result = _run(FakeHerdrClient(), _config(root, retention_days=0), "echo fresh")

    assert result.exit_code == 0  # type: ignore[attr-defined]
    assert os.path.isdir(result.spool.directory)  # type: ignore[attr-defined]


def test_meta_records_the_identity_the_reaper_needs(tmp_path: object) -> None:
    """A run record with only a shell PID can never authorise closing anything.

    ``herdr_run.reap`` requires ``(pid, boot_id, start_ticks)`` before it will call a tab stale, so
    a writer that records the pid alone makes the whole reaper inert -- it would answer UNKNOWN for
    every pane forever, and "reaped 0" would look exactly like a healthy workspace.
    """
    root = str(tmp_path)
    config = _config(root)
    fake = FakeHerdrClient()
    # Report our own pid as the pane shell, so the recorded identity binds against a real /proc.
    fake.shell_pids["w1:p1"] = os.getpid()
    result = cast(RunResult, _run(fake, config, "echo identity"))
    meta_path = write_meta(result, admit("echo identity", config), config, "agent")

    with open(meta_path, encoding="utf-8") as handle:
        document = cast(dict[str, object], json.load(handle))
    readiness = cast(dict[str, object], document["readiness"])
    flags, identity = evidence_from_runs(cast(str, document["pane_id"]), [document])
    assert readiness["shell_pid"] == os.getpid()
    assert isinstance(readiness["boot_id"], str) and readiness["boot_id"]
    assert isinstance(readiness["shell_start_ticks"], int)
    assert flags == (True,)
    assert identity is not None and identity.is_bound()

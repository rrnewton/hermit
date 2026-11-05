"""Exact Herdr allocation commands used by first-class agentctl launches."""
from __future__ import annotations

import errno
import io
import json
import os
import select
import shutil
import shlex
import signal
import subprocess
import sys
import time
from collections.abc import Sequence
from dataclasses import replace
from pathlib import Path

import agentctl.client as client_module
from agentctl.client import (
    AgentIdentity, AgentPaneInfo, CustomProcessIdentity, Pane,
    HerdrClient,
    ProcessInfo,
    muse_auto_review_idle_composer,
    muse_prompt_in_composer,
    muse_prompt_in_transcript,
    muse_prompt_is_exact_composer,
    muse_prompt_transcript_count,
    muse_startup_metadata,
    muse_verified_process_composer,
    muse_verified_process_goal_paused,
    muse_verified_process_idle_composer,
    muse_verified_process_prompt_in_composer,
    muse_verified_process_prompt_is_exact_composer,
    muse_verified_process_prompt_transcript_count,
)
from agentctl.errors import HerdrUnavailable
from agentctl.procstat import ProcessStat, parse_process_stat
import pytest


class Runner:
    def __init__(self, response: dict[str, object]) -> None:
        self.response = response
        self.calls: list[list[str]] = []

    def __call__(self, command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(list(command))
        return subprocess.CompletedProcess(
            command, 0, json.dumps({"result": self.response}), ""
        )


@pytest.mark.parametrize(
    ("descriptor", "limit", "label"),
    [
        (1, client_module._CONTROL_STDOUT_BYTES, "stdout"),
        (2, client_module._CONTROL_STDERR_BYTES, "stderr"),
    ],
)
def test_control_capture_refuses_output_above_each_byte_bound(
    descriptor: int, limit: int, label: str,
) -> None:
    command = [
        sys.executable,
        "-c",
        (
            "import os\n"
            f"remaining = {limit} + 1\n"
            "chunk = b'x' * 65536\n"
            "while remaining:\n"
            f"    written = os.write({descriptor}, chunk[:min(remaining, len(chunk))])\n"
            "    remaining -= written\n"
        ),
    ]
    with pytest.raises(OSError, match=f"control {label} exceeds {limit} bytes"):
        client_module._bounded_control_command(command, timeout=10)


_ENVIRONMENT = (
    "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
    "LITERAL=a b=$(unexpanded)=tail",
)


def test_workspace_creation_passes_literal_environment_before_launch() -> None:
    runner = Runner({
        "workspace": {"workspace_id": "w1"},
        "tab": {"tab_id": "t1"},
        "root_pane": {"pane_id": "p1"},
    })
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    assert client.create_workspace(
        label="subagents", cwd="/work/project", environment=_ENVIRONMENT
    ) == ("w1", "t1", "p1")
    assert runner.calls == [[
        "fixture-herdr", "workspace", "create", "--label", "subagents",
        "--cwd", "/work/project", "--env", _ENVIRONMENT[0],
        "--env", _ENVIRONMENT[1], "--no-focus",
    ]]


def test_tab_creation_passes_literal_environment_before_launch() -> None:
    runner = Runner({
        "tab": {"tab_id": "t1"},
        "root_pane": {"pane_id": "p1", "tab_id": "t1", "workspace_id": "w1"},
    })
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    assert client.create_tab_with_pane(
        workspace_id="w1", label="worker", cwd="/work/project",
        environment=_ENVIRONMENT,
    ) == ("t1", "p1")
    assert runner.calls == [[
        "fixture-herdr", "tab", "create", "--workspace", "w1",
        "--label", "worker", "--cwd", "/work/project",
        "--env", _ENVIRONMENT[0], "--env", _ENVIRONMENT[1], "--no-focus",
    ]]


def test_pane_move_captures_reassigned_identity_and_exact_destination() -> None:
    runner = Runner({
        "move_result": {
            "changed": True,
            "previous_pane_id": "w1:p1",
            "pane": {
                "pane_id": "w2:p2", "tab_id": "w2:t2", "workspace_id": "w2",
            },
        },
    })
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    assert client.move_pane_to_new_tab(
        "w1:p1", workspace_id="w2", tab_label="worker",
    ) == Pane("w2:p2", "w2:t2", "w2")
    assert runner.calls == [[
        "fixture-herdr", "pane", "move", "w1:p1", "--new-tab",
        "--workspace", "w2", "--label", "worker", "--no-focus",
    ]]


def test_pane_move_refuses_response_for_different_destination() -> None:
    runner = Runner({
        "move_result": {
            "changed": True,
            "previous_pane_id": "w1:p1",
            "pane": {
                "pane_id": "w3:p2", "tab_id": "w3:t2", "workspace_id": "w3",
            },
        },
    })
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    with pytest.raises(HerdrUnavailable, match="expected 'w2'"):
        client.move_pane_to_new_tab(
            "w1:p1", workspace_id="w2", tab_label="worker",
        )


@pytest.mark.parametrize(
    ("changed", "previous", "message"),
    ((False, "w1:p1", "no move occurred"),
     (True, "wrong", "previous pane identity")),
)
def test_pane_move_refuses_incomplete_transition_proof(
    changed: bool, previous: str, message: str,
) -> None:
    runner = Runner({
        "move_result": {
            "changed": changed,
            "previous_pane_id": previous,
            "pane": {
                "pane_id": "w2:p2", "tab_id": "w2:t2", "workspace_id": "w2",
            },
        },
    })
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    with pytest.raises(HerdrUnavailable, match=message):
        client.move_pane_to_new_tab(
            "w1:p1", workspace_id="w2", tab_label="worker",
        )


def _custom_runner(
    screen: str, *, observed_executable: str | None = None,
) -> tuple[HerdrClient, list[list[str]]]:
    calls: list[list[str]] = []
    executable = os.path.realpath("/bin/true")
    observed = observed_executable or executable

    def run(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        argv = list(command)
        calls.append(argv)
        if argv[1:3] == ["pane", "process-info"]:
            output = {"result": {"process_info": {
                "pane_id": "p1", "shell_pid": 10, "foreground_process_group_id": 11,
                "foreground_processes": [{
                    "pid": 2_147_483_647, "name": "true", "cmdline": f"{observed} literal",
                    "argv": [observed, "literal"], "executable": observed,
                }],
            }}}
            return subprocess.CompletedProcess(argv, 0, json.dumps(output), "")
        if argv[1:3] == ["pane", "read"]:
            return subprocess.CompletedProcess(argv, 0, screen, "")
        if argv[1:3] == ["pane", "get"]:
            output = {"result": {"pane": {
                "pane_id": "p1", "workspace_id": "w1", "cwd": "/work/project",
                "agent": "muse", "agent_status": "idle", "agent_session": None,
            }}}
            return subprocess.CompletedProcess(argv, 0, json.dumps(output), "")
        return subprocess.CompletedProcess(argv, 0, "", "")

    client = HerdrClient(herdr_bin="fixture-herdr", run=run)
    client._harness_executable = lambda _kind: executable  # type: ignore[assignment]
    return client, calls


def test_custom_muse_launch_uses_literal_shell_quoting_and_exact_pane_report() -> None:
    screen = (
        "Muse Code 1.3.0\n\n"
        "reasoning effort ultra is not available (gate ultra_reasoning_effort is closed); using xhigh\n"
        "────────────────\n❯\n────────────────\n"
        "watermelon-preview · xhigh · /work/project · Auto-review\n"
    )
    client, calls = _custom_runner(screen)
    arguments = ("--reasoning-effort", "ultra", 'literal $(unexpanded) "quotes"')
    client.start_pane_agent("worker", "muse", "p1", arguments, timeout=1)
    assert muse_startup_metadata(screen) == (
        "reasoning effort ultra is not available (gate ultra_reasoning_effort is closed); using xhigh",
        "xhigh",
    )
    pane_run = next(call for call in calls if call[1:3] == ["pane", "run"])
    assert shlex.split(pane_run[4]) == [os.path.realpath("/bin/true"), *arguments]
    report = next(call for call in calls if call[1:3] == ["pane", "report-agent"])
    assert report[3:] == [
        "p1", "--source", "agentctl", "--agent", "muse", "--state", "idle",
        "--message", "agentctl custom harness",
    ]


def test_current_muse_yolo_footer_is_an_idle_composer() -> None:
    screen = (
        "Muse Code at Meta (https://fb.workplace.com/groups/27315719428107177)\n"
        "Using AI Gateway (Meta Model API upstream)\n\n"
        "  Muse Code 1.4.0\n\n"
        "────────────────\n❯\n"
        "────────────────\n"
        "  kiki_gb300_mxfp8_6p2_840_nwr · xhigh · /work/project · YOLO\n"
    )
    assert client_module.muse_idle_composer(screen)
    assert not muse_auto_review_idle_composer(
        screen.replace("Using AI Gateway", "Auto-review was previously enabled")
    )
    ambiguous = screen.replace(
        "  kiki_gb300_mxfp8_6p2_840_nwr · xhigh · /work/project · YOLO\n",
        "  kiki · xhigh · /work/project · Auto-review\n"
        "  kiki_gb300_mxfp8_6p2_840_nwr · xhigh · /work/project · YOLO\n",
    )
    assert not client_module.muse_idle_composer(ambiguous)
    assert not muse_auto_review_idle_composer(ambiguous)
    choice = screen.replace("❯\n", "❯ Yes, continue\n  No, exit\n")
    assert not client_module.muse_idle_composer(choice)


def test_verified_headerless_muse_composer_distinguishes_buffered_input() -> None:
    divider = "─" * 40
    footer = "kiki · xhigh · /work/project · YOLO\n"
    idle = f"old transcript\n{divider}\n❯\n{divider}\n{footer}"
    assert not client_module.muse_idle_composer(idle)
    assert muse_verified_process_idle_composer(idle)
    assert muse_verified_process_composer(idle)

    prompt = "require the full deterministic-scheduling-review skill"
    staged = idle.replace("❯\n", f"❯ {prompt}\n")
    assert not muse_prompt_is_exact_composer(staged, prompt)
    assert muse_verified_process_prompt_is_exact_composer(staged, prompt)
    assert muse_verified_process_prompt_in_composer(staged, prompt)
    accepted = idle.replace(
        "old transcript\n", f"❯ {prompt}\n◆ Working\n",
    )
    assert muse_verified_process_prompt_transcript_count(accepted, prompt) == 1


def test_verified_muse_paused_goal_requires_footer_region() -> None:
    divider = "─" * 40
    footer = "kiki · xhigh · /work/project · YOLO"
    paused = (
        f"Muse Code 1.4.0\n{divider}\n❯\n{divider}\n"
        f"Goal (paused)\n{footer}\n"
    )
    transcript_echo = (
        f"Muse Code 1.4.0\nGoal (paused)\n{divider}\n❯\n{divider}\n{footer}\n"
    )
    assert muse_verified_process_goal_paused(paused)
    assert not muse_verified_process_goal_paused(transcript_echo)


def test_muse_prompt_must_move_from_composer_to_transcript() -> None:
    prompt = "literal $(unexpanded) delivery\nsecond line"
    header = "Muse Code 1.3.0\n"
    divider = "────────────────\n"
    footer = "watermelon-preview · xhigh · /work/project · Auto-review\n"
    staged = header + divider + f"❯ {prompt}\n" + divider + footer
    accepted = (
        header + f"❯ {prompt}\nWorking...\n" + divider + "❯\n" + divider + footer
    )
    accepted_current = (
        header + f"❯ {prompt}\n◆ Working...\n" + divider + "❯\n" + divider + footer
    )
    error_redraw = header + divider + f"❯ {prompt}\nError: retry\n" + divider + footer
    assert muse_prompt_in_composer(staged, prompt)
    assert not muse_prompt_in_transcript(staged, prompt)
    assert muse_prompt_in_transcript(accepted, prompt)
    assert muse_prompt_in_transcript(accepted_current, prompt)
    assert not muse_prompt_in_composer(accepted, prompt)
    assert muse_prompt_in_composer(error_redraw, prompt)
    assert not muse_prompt_in_transcript(error_redraw, prompt)
    repeated_staged = (
        header + f"❯ {prompt}\n◆ prior answer\n" + divider
        + f"❯ {prompt}\n" + divider + footer
    )
    cleared_without_submit = (
        header + f"❯ {prompt}\n◆ prior answer\n" + divider + "❯\n" + divider + footer
    )
    assert muse_prompt_transcript_count(repeated_staged, prompt) == 1
    assert muse_prompt_transcript_count(cleared_without_submit, prompt) == 1


def test_custom_muse_launch_never_accepts_a_trust_prompt() -> None:
    client, calls = _custom_runner("Do you trust this workspace?\n❯ Yes\n  No\n")
    with pytest.raises(HerdrUnavailable, match="trust prompt.*no input"):
        client.start_pane_agent("worker", "muse", "p1", (), timeout=1)
    assert not any(call[1:3] == ["pane", "report-agent"] for call in calls)


def test_custom_muse_launch_rejects_bare_prompt_without_idle_footer() -> None:
    client, calls = _custom_runner("A choice dialog\n❯\n")
    with pytest.raises(HerdrUnavailable, match="verified idle composer"):
        client.start_pane_agent("worker", "muse", "p1", (), timeout=0.01)
    assert not any(call[1:3] == ["pane", "report-agent"] for call in calls)


def test_custom_harness_missing_executable_is_a_refusal() -> None:
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner({}))
    with pytest.raises(HerdrUnavailable, match="not found"):
        client._harness_executable("definitely-not-installed-agentctl-harness")


def test_custom_muse_explicit_executable_is_absolute_and_validated(tmp_path: Path) -> None:
    executable = tmp_path / "muse"
    shutil.copyfile("/bin/true", executable)
    executable.chmod(0o700)
    client = HerdrClient(
        herdr_bin="fixture-herdr",
        run=Runner({}),
        environ={"AGENTCTL_MUSE_BIN": str(executable)},
    )
    assert client._harness_executable("muse") == str(executable.resolve())

    relative = HerdrClient(
        herdr_bin="fixture-herdr",
        run=Runner({}),
        environ={"AGENTCTL_MUSE_BIN": "muse"},
    )
    with pytest.raises(HerdrUnavailable, match="must be an absolute path"):
        relative._harness_executable("muse")

    script = tmp_path / "muse-script"
    script.write_text("#!/bin/sh\n", encoding="utf-8")
    script.chmod(0o700)
    scripted = HerdrClient(
        herdr_bin="fixture-herdr", run=Runner({}),
        environ={"AGENTCTL_MUSE_BIN": str(script)},
    )
    with pytest.raises(HerdrUnavailable, match="native ELF"):
        scripted.start_pane_agent("worker", "muse", "p1", (), timeout=1)


def test_custom_harness_rejects_same_name_at_a_different_executable_path() -> None:
    client, _calls = _custom_runner(
        "────────────────\n❯\n────────────────\nAuto-review\n",
        observed_executable="/tmp/lookalike/true",
    )
    with pytest.raises(HerdrUnavailable, match="not the foreground process"):
        client.verify_custom_harness("p1", "muse")


def test_custom_harness_rejects_matching_report_when_kernel_executable_differs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, _calls = _custom_runner(
        "────────────────\n❯\n────────────────\nAuto-review\n",
    )
    monkeypatch.setattr(
        client, "_process_executable", lambda _pid: os.path.realpath("/bin/false")
    )
    with pytest.raises(HerdrUnavailable, match="not the foreground process"):
        client.verify_custom_harness("p1", "muse")


def _suggestion13_process_response(foreground: dict[str, object]) -> dict[str, object]:
    return {"process_info": {
        "pane_id": "p1", "shell_pid": 10, "foreground_process_group_id": 11,
        "foreground_processes": [foreground],
    }}


@pytest.mark.parametrize("variant", ["absent", "null", "empty"])
def test_suggestion13_missing_foreground_argv_remains_unknown_metadata(variant: str) -> None:
    foreground: dict[str, object] = {
        "pid": 2_147_483_647, "name": "muse", "cmdline": os.path.realpath("/bin/true"),
        "executable": os.path.realpath("/bin/true"),
    }
    if variant != "absent":
        foreground["argv"] = None if variant == "null" else []
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner(_suggestion13_process_response(foreground)))
    info = client.process_info("p1")
    assert info.foreground == ((2_147_483_647, "muse", os.path.realpath("/bin/true"), "", os.path.realpath("/bin/true")),)
    metadata = os.stat(os.path.realpath("/bin/true"))
    assert client._pane_process_identity(info, os.path.realpath("/bin/true"),
        launch_image=(metadata.st_dev, metadata.st_ino)) is None
    assert client._idle_shell_proof(info) is None


@pytest.mark.parametrize("argv", ["/bin/true", 7, {}, [None], [""], ["/bin/true", 7], ["/bad\0path"]])
def test_suggestion13_supplied_malformed_foreground_argv_still_refuses(argv: object) -> None:
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner(_suggestion13_process_response({
        "pid": 11, "name": "muse", "cmdline": "/bin/true", "argv": argv,
    })))
    with pytest.raises(HerdrUnavailable, match="foreground process argv"):
        client.process_info("p1")


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_suggestion13_unknown_argv_can_verify_only_kernel_generation_or_opened_launch_image(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    executable = tmp_path / "muse"
    shutil.copyfile("/usr/bin/sleep", executable)
    executable.chmod(0o700)
    process = subprocess.Popen([str(executable), "30"], start_new_session=True)
    try:
        foreground: dict[str, object] = {
            "pid": process.pid, "name": "untrusted-name", "cmdline": "untrusted-command", "argv": None,
            "executable": "/untrusted/reported-image",
        }
        response = _suggestion13_process_response(foreground)
        process_info = response["process_info"]
        assert isinstance(process_info, dict)
        process_info["foreground_process_group_id"] = process.pid
        runner = Runner(response)
        client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
        info = client.process_info("p1")
        identity = client.harness_identity("p1", "muse")
        assert identity is not None and client.verify_harness_identity("p1", identity)
        assert client._pane_process_identity(info, str(executable)) is None
        image = executable.stat()
        assert client._pane_process_identity(info, str(executable),
            launch_image=(image.st_dev, image.st_ino)) == identity
        assert client._pane_process_identity(info, str(executable),
            launch_image=(image.st_dev, image.st_ino + 1)) is None
        altered = (
            replace(identity, pid=identity.pid + 1), replace(identity, starttime_ticks=identity.starttime_ticks + 1),
            replace(identity, boot_id="ffffffff-ffff-ffff-ffff-ffffffffffff"),
            replace(identity, executable_device=identity.executable_device + 1),
            replace(identity, executable_inode=identity.executable_inode + 1),
        )
        assert all(not client.verify_harness_identity("p1", expected) for expected in altered)
        process_info["foreground_process_group_id"] = process.pid + 1
        assert not client.verify_harness_identity("p1", identity)
        process_info["foreground_process_group_id"] = process.pid

        screen = "Muse Code 1.3.0\n────────────────\n❯\n────────────────\nmodel · high · /work · Auto-review\n"

        def run(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
            if list(command)[1:3] == ["pane", "read"]:
                runner.calls.append(list(command))
                return subprocess.CompletedProcess(command, 0, screen, "")
            return runner(command)

        monkeypatch.setattr(client, "_harness_executable", lambda _kind: str(executable))
        monkeypatch.setattr(client, "_run", run)
        observed: list[CustomProcessIdentity] = []
        assert client.start_pane_agent("worker", "muse", "p1", (), timeout=1,
            on_observed=observed.append) == identity
        assert observed == [identity]
    finally:
        process.terminate()
        process.wait(timeout=5)


_SUGGESTION13_CODEX_INFO = AgentPaneInfo(
    "p1", "w1", "/work", "codex", "unknown", "codex", "native", "terminal", "t1",
)


def _suggestion13_codex_readiness(
    monkeypatch: pytest.MonkeyPatch,
) -> tuple[HerdrClient, AgentPaneInfo, CustomProcessIdentity, list[str]]:
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner({}))
    info = _SUGGESTION13_CODEX_INFO
    identity = CustomProcessIdentity(1, "00000000-0000-0000-0000-000000000000", 11, 11, 1, 2)
    checks: list[str] = []
    monkeypatch.setattr(client, "harness_identity", lambda _pane, _kind: identity)
    monkeypatch.setattr(client, "verify_harness_identity", lambda _pane, expected: expected == identity)
    monkeypatch.setattr(client, "pane_info", lambda _pane: info)

    def screen(_pane: str) -> str:
        checks.append("screen")
        return "› \x1b[2mAsk Codex to do anything\x1b[0m\n  GPT default · /work\n"

    monkeypatch.setattr(client, "read_screen", screen)
    return client, info, identity, checks


def test_suggestion13_codex_unknown_readiness_keeps_raw_status_and_requires_process_proof(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, _identity, checks = _suggestion13_codex_readiness(monkeypatch)
    assert client.codex_idle_ready(info) and info.status == "unknown"
    assert checks == ["screen"]
    monkeypatch.setattr(client, "harness_identity", lambda _pane, _kind: None)
    assert not client.codex_idle_ready(info) and checks == ["screen"]


@pytest.mark.parametrize("changed", [
    replace(_SUGGESTION13_CODEX_INFO, agent="claude"),
    replace(_SUGGESTION13_CODEX_INFO, status="idle"),
    replace(_SUGGESTION13_CODEX_INFO, status="working"),
    replace(_SUGGESTION13_CODEX_INFO, status="blocked"),
    replace(_SUGGESTION13_CODEX_INFO, terminal_id=None),
    replace(_SUGGESTION13_CODEX_INFO, terminal_id=""),
    replace(_SUGGESTION13_CODEX_INFO, terminal_id="bad\0terminal"),
    replace(_SUGGESTION13_CODEX_INFO, session_agent=None),
    replace(_SUGGESTION13_CODEX_INFO, session_agent="muse"),
    replace(_SUGGESTION13_CODEX_INFO, session_value=None),
    replace(_SUGGESTION13_CODEX_INFO, session_value=""),
    replace(_SUGGESTION13_CODEX_INFO, session_value="bad\0session"),
])
def test_suggestion13_codex_unknown_readiness_rejects_ineligible_live_metadata(
    changed: AgentPaneInfo, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, _info, _identity, checks = _suggestion13_codex_readiness(monkeypatch)
    assert not client.codex_idle_ready(changed) and checks == []


@pytest.mark.parametrize("changed", [
    replace(_SUGGESTION13_CODEX_INFO, pane_id="p2"),
    replace(_SUGGESTION13_CODEX_INFO, workspace_id="w2"),
    replace(_SUGGESTION13_CODEX_INFO, cwd="/other"),
    replace(_SUGGESTION13_CODEX_INFO, agent="claude"),
    replace(_SUGGESTION13_CODEX_INFO, status="working"),
    replace(_SUGGESTION13_CODEX_INFO, session_agent="muse"),
    replace(_SUGGESTION13_CODEX_INFO, session_value="replacement"),
    replace(_SUGGESTION13_CODEX_INFO, terminal_id="replacement"),
    replace(_SUGGESTION13_CODEX_INFO, tab_id="other-tab"),
])
def test_suggestion13_codex_unknown_readiness_rejects_pane_changes_during_screen_read(
    changed: AgentPaneInfo, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, _identity, _checks = _suggestion13_codex_readiness(monkeypatch)
    monkeypatch.setattr(client, "pane_info", lambda _pane: changed)
    assert not client.codex_idle_ready(info)


def test_suggestion13_codex_unknown_readiness_rejects_process_change_during_screen_read(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, _identity, _checks = _suggestion13_codex_readiness(monkeypatch)
    monkeypatch.setattr(client, "verify_harness_identity", lambda _pane, _expected: False)
    assert not client.codex_idle_ready(info)


_SUGGESTION13_START_TIMEOUT = (
    '{"id":"cli:agent:start","error":{"code":"timeout",'
    '"message":"timed out waiting for agent startup"}}'
)


def _suggestion13_timeout_client(
    monkeypatch: pytest.MonkeyPatch, *, returncode: int = 1,
    stdout: str = "", stderr: str = _SUGGESTION13_START_TIMEOUT,
) -> tuple[HerdrClient, AgentPaneInfo, list[list[str]], list[str]]:
    client, info, _identity, screens = _suggestion13_codex_readiness(monkeypatch)
    initial = replace(info, agent=None, session_agent=None, session_value=None)
    calls: list[list[str]] = []

    def run(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        calls.append(list(command))
        return subprocess.CompletedProcess(command, returncode, stdout, stderr)

    monkeypatch.setattr(client, "_run", run)
    monkeypatch.setattr(client, "pane_info", lambda _pane: info if calls else initial)
    monkeypatch.setattr(client, "agent_identity", lambda name: AgentIdentity(
        name, info.pane_id, info.tab_id, info.terminal_id,
    ))
    return client, info, calls, screens


def test_suggestion13_exact_codex_start_timeout_accepts_verified_unknown_idle_once(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, calls, screens = _suggestion13_timeout_client(monkeypatch)
    client.start_agent("worker", "codex", info.pane_id, ("--resume", "native"), timeout=1)
    assert calls == [["fixture-herdr", "agent", "start", "worker", "--kind", "codex",
                      "--pane", "p1", "--timeout", "1000", "--", "--resume", "native"]]
    assert screens == ["screen"] and client.pane_info(info.pane_id).status == "unknown"


@pytest.mark.parametrize("returncode,stdout,stderr", [
    (2, "", _SUGGESTION13_START_TIMEOUT),
    (1, "unexpected output", _SUGGESTION13_START_TIMEOUT),
    (1, "", "timed out waiting for agent startup"),
    (1, "", _SUGGESTION13_START_TIMEOUT.replace('"timeout"', '"agent_not_ready"')),
    (1, "", _SUGGESTION13_START_TIMEOUT.replace('"cli:agent:start"', '"cli:agent:start:timeout"')),
    (1, "", _SUGGESTION13_START_TIMEOUT.replace('"timeout"', '"timeout","extra":true')),
    (1, "", _SUGGESTION13_START_TIMEOUT.replace('"timeout"', '"timeout","code":"timeout"')),
    (1, "", _SUGGESTION13_START_TIMEOUT.replace('"cli:agent:start"', '"cli:agent:start","extra":true')),
])
def test_suggestion13_other_start_errors_never_use_codex_readiness_fallback(
    returncode: int, stdout: str, stderr: str, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, calls, screens = _suggestion13_timeout_client(
        monkeypatch, returncode=returncode, stdout=stdout, stderr=stderr,
    )
    with pytest.raises(HerdrUnavailable, match="agent start 'worker'"):
        client.start_agent("worker", "codex", info.pane_id, timeout=1)
    assert len(calls) == 1 and screens == []


@pytest.mark.parametrize("failure", [
    "draft", "busy", "trust", "process", "name", "terminal", "workspace", "native",
    "occupied-before-start", "final-name",
])
def test_suggestion13_exact_timeout_still_requires_coherent_idle_runtime(
    failure: str, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, calls, _screens = _suggestion13_timeout_client(monkeypatch)
    if failure in ("draft", "busy", "trust"):
        screen = {
            "draft": "› human draft\n  GPT · /work\n",
            "busy": "›\n  esc to interrupt\n  GPT · /work\n",
            "trust": "Trust this folder?\n›\n  GPT · /work\n",
        }[failure]
        monkeypatch.setattr(client, "read_screen", lambda _pane: screen)
    elif failure == "process":
        monkeypatch.setattr(client, "verify_harness_identity", lambda _pane, _expected: False)
    elif failure in ("name", "final-name"):
        named_calls = 0

        def named(name: str) -> AgentIdentity:
            nonlocal named_calls
            named_calls += 1
            wrong = failure == "name" or named_calls > 1
            return AgentIdentity(name, "other" if wrong else "p1", "t1", "terminal")

        monkeypatch.setattr(client, "agent_identity", named)
    else:
        initial = replace(info, agent=None, session_agent=None, session_value=None)
        changed = info
        if failure == "terminal":
            changed = replace(info, terminal_id="replacement")
        elif failure == "workspace":
            changed = replace(info, workspace_id="other")
        elif failure == "native":
            changed = replace(info, session_agent="muse")
        else:
            initial = info
        monkeypatch.setattr(client, "pane_info", lambda _pane: changed if calls else initial)
    with pytest.raises(HerdrUnavailable, match="timed out waiting for agent startup"):
        client.start_agent("worker", "codex", info.pane_id, timeout=1)
    assert len(calls) == 1


@pytest.mark.parametrize("returncode", [0, 1])
def test_suggestion13_unavailable_optional_snapshot_preserves_native_result(
    returncode: int, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, calls, screens = _suggestion13_timeout_client(monkeypatch, returncode=returncode)

    def unavailable(_pane: str) -> AgentPaneInfo:
        raise HerdrUnavailable("optional snapshot unavailable")

    monkeypatch.setattr(client, "pane_info", unavailable)
    if returncode == 0:
        client.start_agent("worker", "codex", info.pane_id, timeout=1)
    else:
        with pytest.raises(HerdrUnavailable, match="timed out waiting for agent startup"):
            client.start_agent("worker", "codex", info.pane_id, timeout=1)
    assert len(calls) == 1 and screens == []


def test_suggestion13_claude_start_timeout_keeps_existing_refusal(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, info, calls, screens = _suggestion13_timeout_client(monkeypatch)
    with pytest.raises(HerdrUnavailable, match="timed out waiting for agent startup"):
        client.start_agent("worker", "claude", info.pane_id, timeout=1)
    assert len(calls) == 1 and screens == []


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_recorded_pane_shell_identity_rejects_every_generation_change() -> None:
    executable = os.path.realpath("/bin/bash")
    shell = subprocess.Popen(
        [executable, "--noprofile", "--norc"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        foreground: dict[str, object] = {
            "pid": shell.pid,
            "name": "bash",
            "cmdline": executable,
            "argv": [executable],
            "executable": executable,
        }
        process_info: dict[str, object] = {
            "pane_id": "p1",
            "shell_pid": shell.pid,
            "foreground_process_group_id": shell.pid,
            "foreground_processes": [foreground],
        }
        runner = Runner({"process_info": process_info})
        client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
        identity = client.pane_shell_identity("p1")

        assert client.pane_is_same_idle_shell("p1", identity)
        mismatches = (
            replace(identity, boot_id="ffffffff-ffff-ffff-ffff-ffffffffffff"),
            replace(identity, pid=identity.pid + 1),
            replace(identity, starttime_ticks=identity.starttime_ticks + 1),
            replace(identity, executable_device=identity.executable_device + 1),
            replace(identity, executable_inode=identity.executable_inode + 1),
        )
        assert all(
            not client.pane_is_same_idle_shell("p1", item) for item in mismatches
        )

        foreground["argv"] = [os.path.realpath("/usr/bin/sleep")]
        assert not client.pane_is_same_idle_shell("p1", identity)
    finally:
        shell.terminate()
        shell.wait(timeout=5)


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_real_non_shell_process_cannot_be_adopted_as_a_pane_shell() -> None:
    executable = os.path.realpath("/usr/bin/sleep")
    process = subprocess.Popen([executable, "30"], start_new_session=True)
    try:
        process_info: dict[str, object] = {
            "pane_id": "p1",
            "shell_pid": process.pid,
            "foreground_process_group_id": process.pid,
            "foreground_processes": [{
                "pid": process.pid,
                "name": "sleep",
                "cmdline": f"{executable} 30",
                "argv": [executable, "30"],
                "executable": executable,
            }],
        }
        client = HerdrClient(
            herdr_bin="fixture-herdr", run=Runner({"process_info": process_info})
        )
        observed = client._process_identity(process.pid)
        assert observed is not None
        with pytest.raises(HerdrUnavailable, match="supported identity-bound shell"):
            client.pane_shell_identity("p1")
        assert not client.pane_is_idle_shell("p1")
        assert not client.pane_is_same_idle_shell("p1", observed[0])
    finally:
        process.terminate()
        process.wait(timeout=5)


def test_process_stat_parser_treats_comm_as_opaque_bytes() -> None:
    fields = [b"S", b"123", b"888", b"999", *([b"0"] * 15), b"987654321"]
    raw = b"456 (p\xff)\n(\x80) " + b" ".join(fields) + b"\n"
    parsed = parse_process_stat(raw)
    assert parsed is not None
    assert (
        parsed.pid,
        parsed.state,
        parsed.ppid,
        parsed.pgrp,
        parsed.session,
        parsed.starttime,
    ) == (456, "S", 123, 888, 999, 987654321)


@pytest.mark.parametrize(
    "raw",
    [
        b"456 (unterminated S 1 2 3",
        b"456 (x)S 1 2 3",
        b"456 (x) S 1 2 3",
    ],
)
def test_process_stat_parser_refuses_missing_or_truncated_boundaries(raw: bytes) -> None:
    assert parse_process_stat(raw) is None


def test_descendant_census_accepts_an_unrelated_opaque_comm(tmp_path: Path) -> None:
    ready = tmp_path / "opaque-comm-ready"
    script = r'''
import ctypes
import pathlib
import sys
import time
libc = ctypes.CDLL(None, use_errno=True)
if libc.prctl(15, ctypes.c_char_p(b"p\xff)\n(\x80"), 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "PR_SET_NAME")
pathlib.Path(sys.argv[1]).write_text("ready")
time.sleep(60)
'''
    leaf = subprocess.Popen((os.path.realpath("/usr/bin/sleep"), "60"))
    unrelated = subprocess.Popen((sys.executable, "-c", script, str(ready)))
    try:
        deadline = time.monotonic() + 2
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(0.005)
        assert ready.exists()
        assert HerdrClient._process_has_no_descendants(leaf.pid)
    finally:
        leaf.kill()
        unrelated.kill()
        leaf.wait(timeout=2)
        unrelated.wait(timeout=2)


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_process_identity_maps_pidfd_close_failure_to_unavailable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    executable = os.path.realpath("/usr/bin/sleep")
    process = subprocess.Popen([executable, "30"], start_new_session=True)
    real_close = os.close
    injected = False

    def close_then_fail(descriptor: int) -> None:
        nonlocal injected
        real_close(descriptor)
        if not injected:
            injected = True
            raise OSError(errno.EIO, "injected pidfd close failure")

    try:
        monkeypatch.setattr("agentctl.client.os.close", close_then_fail)
        with pytest.raises(HerdrUnavailable, match="cannot close pidfd"):
            HerdrClient._process_identity(process.pid)
        assert injected is True
    finally:
        process.terminate()
        process.wait(timeout=5)


_LivenessFixture = tuple[HerdrClient, CustomProcessIdentity, ProcessStat]


def _liveness_identity() -> CustomProcessIdentity:
    return CustomProcessIdentity(1, "00000000-0000-0000-0000-000000000000", 4242, 99, 10, 11)


@pytest.fixture
def liveness_probe(monkeypatch: pytest.MonkeyPatch) -> _LivenessFixture:
    identity = _liveness_identity()
    observed = ProcessStat(pid=4242, starttime=99, ppid=1, pgrp=4242, session=4242, state="S")
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner({}))
    monkeypatch.setattr("agentctl.client.os.pidfd_open", lambda _pid: os.open(os.devnull, os.O_RDONLY), raising=False)
    monkeypatch.setattr(client, "_liveness_boot_identity", lambda: identity.boot_id)
    monkeypatch.setattr(client, "_liveness_process_stat", lambda _pid: observed)
    monkeypatch.setattr(client, "_liveness_executable_identity", lambda _pid: (10, 11))
    monkeypatch.setattr(client, "_pidfd_exited", lambda _descriptor: False)
    return client, identity, observed


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_process_liveness_tracks_a_real_child_before_and_after_kill() -> None:
    runner = Runner({})
    client = HerdrClient(herdr_bin="fixture-herdr", run=runner)
    process = subprocess.Popen([os.path.realpath("/usr/bin/sleep"), "30"])
    descriptor = os.pidfd_open(process.pid)
    try:
        observed = client._process_identity(process.pid)
        assert observed is not None
        identity = observed[0]
        assert client.process_liveness(identity) == "alive"
        process.kill()
        poller = select.poll()
        poller.register(descriptor, select.POLLIN)
        assert poller.poll(2000), "the owned child did not exit"
        # Do not reap yet: the pidfd proves death even while /proc still has a zombie.
        assert client.process_liveness(identity) == "dead"
        process.wait(timeout=5)
        assert client.process_liveness(identity) == "dead"
        assert runner.calls == []
    finally:
        os.close(descriptor)
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)


@pytest.mark.parametrize(
    ("identity", "outcome"),
    [
        (replace(_liveness_identity(), boot_id="ffffffff-ffff-ffff-ffff-ffffffffffff"), "dead"),
        (replace(_liveness_identity(), starttime_ticks=98), "dead"),
        (replace(_liveness_identity(), executable_device=9), "unknown"),
        (replace(_liveness_identity(), executable_inode=12), "unknown"),
        (replace(_liveness_identity(), version=0), "unknown"),
        (replace(_liveness_identity(), boot_id="invalid"), "unknown"),
        (replace(_liveness_identity(), pid=0), "unknown"),
        (replace(_liveness_identity(), pid=2_147_483_648), "unknown"),
        (replace(_liveness_identity(), starttime_ticks=0), "unknown"),
        (replace(_liveness_identity(), executable_inode=0), "unknown"),
    ],
)
def test_process_liveness_distinguishes_generations_from_live_image_changes(
    liveness_probe: _LivenessFixture, identity: CustomProcessIdentity, outcome: str,
) -> None:
    client, _recorded, _observed = liveness_probe
    assert client.process_liveness(identity) == outcome


@pytest.mark.parametrize("state", ["Z", "X", "x"])
def test_process_liveness_does_not_require_a_dead_process_executable(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch, state: str,
) -> None:
    client, identity, observed = liveness_probe
    monkeypatch.setattr(client, "_liveness_process_stat", lambda _pid: replace(observed, state=state))

    def no_executable(_pid: int) -> None:
        raise AssertionError("a dead process has no readable executable")

    monkeypatch.setattr(client, "_liveness_executable_identity", no_executable)
    assert client.process_liveness(identity) == "dead"


@pytest.mark.parametrize(
    ("error_number", "outcome"),
    [(errno.ESRCH, "dead"), *[(value, "unknown") for value in (
        errno.EPERM, errno.EACCES, errno.ENOSYS, errno.EINVAL, errno.EIO, errno.EINTR,
    )]],
)
def test_process_liveness_requires_a_positive_pidfd_absence_error(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch,
    error_number: int, outcome: str,
) -> None:
    client, identity, _observed = liveness_probe
    attempts = 0

    def unavailable(_pid: int) -> int:
        nonlocal attempts
        attempts += 1
        raise OSError(error_number, "injected pidfd error")

    monkeypatch.setattr("agentctl.client.os.pidfd_open", unavailable)
    assert client.process_liveness(identity) == outcome
    assert attempts == (4 if error_number == errno.EINTR else 1)


def test_process_liveness_without_pidfds_is_unknown(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, identity, _observed = liveness_probe
    monkeypatch.delattr("agentctl.client.os.pidfd_open")
    assert client.process_liveness(identity) == "unknown"


def test_process_liveness_keeps_pidfd_close_failure_unknown(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, identity, _observed = liveness_probe
    close = os.close

    def close_then_fail(descriptor: int) -> None:
        close(descriptor)
        raise OSError(errno.EIO, "injected close failure")

    monkeypatch.setattr("agentctl.client.os.close", close_then_fail)
    assert client.process_liveness(identity) == "unknown"


def test_process_liveness_retries_interrupted_pidfd_open(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch,
) -> None:
    client, identity, _observed = liveness_probe
    calls = 0

    def interrupted(_pid: int) -> int:
        nonlocal calls
        calls += 1
        if calls == 1:
            raise InterruptedError(errno.EINTR, "injected interrupt")
        return os.open(os.devnull, os.O_RDONLY)

    monkeypatch.setattr("agentctl.client.os.pidfd_open", interrupted)
    assert client.process_liveness(identity) == "alive"
    assert calls == 2


@pytest.mark.parametrize("method", [
    "_liveness_boot_identity", "_liveness_process_stat", "_liveness_executable_identity", "_pidfd_exited",
])
@pytest.mark.parametrize("failure", [errno.EPERM, errno.EIO, errno.ENOENT, None])
def test_process_liveness_keeps_probe_errors_unknown(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch,
    method: str, failure: int | None,
) -> None:
    client, identity, _observed = liveness_probe

    def unavailable(*_args: int) -> None:
        if failure is None:
            raise ValueError("injected malformed probe")
        raise OSError(failure, "injected unreadable probe")

    monkeypatch.setattr(client, method, unavailable)
    assert client.process_liveness(identity) == "unknown"


@pytest.mark.parametrize("changed", ["boot", "stat", "image", "reused-stat", "zombie-stat"])
def test_process_liveness_refuses_changing_probe_snapshots(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch, changed: str,
) -> None:
    client, identity, observed = liveness_probe
    if changed == "boot":
        boot_ids = iter([identity.boot_id, "ffffffff-ffff-ffff-ffff-ffffffffffff"])
        monkeypatch.setattr(client, "_liveness_boot_identity", lambda: next(boot_ids))
    elif changed == "image":
        images = iter([(10, 11), (10, 12)])
        monkeypatch.setattr(client, "_liveness_executable_identity", lambda _pid: next(images))
    else:
        if changed == "reused-stat":
            first, second = replace(observed, starttime=98), observed
        elif changed == "zombie-stat":
            first, second = replace(observed, state="Z"), observed
        else:
            first, second = observed, replace(observed, starttime=100)
        snapshots = iter([first, second])
        monkeypatch.setattr(client, "_liveness_process_stat", lambda _pid: next(snapshots))
    assert client.process_liveness(identity) == "unknown"


@pytest.mark.parametrize("raw", ["", "invalid", "x" * 129, "00000000-0000-0000-0000-000000000000\0"])
def test_process_liveness_refuses_malformed_boot_reads(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch, raw: str,
) -> None:
    client, identity, _observed = liveness_probe

    def boot_file(path: str, *, encoding: str) -> io.StringIO:
        assert path == "/proc/sys/kernel/random/boot_id" and encoding == "ascii"
        return io.StringIO(raw)

    monkeypatch.setattr(client_module, "open", boot_file, raising=False)
    monkeypatch.setattr(client, "_liveness_boot_identity", HerdrClient._liveness_boot_identity)
    assert client.process_liveness(identity) == "unknown"


@pytest.mark.parametrize("raw", [
    b"", b"4242 (unterminated S 1 2 3", b"4242 (x) S 1 2 3", b"x" * 8193,
    b"4241 (x) S 1 1 1 " + b"0 " * 15 + b"99",
    b"4242 (x) S 1 1 1 " + b"0 " * 16,
])
def test_process_liveness_refuses_malformed_stat_reads(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch, raw: bytes,
) -> None:
    client, identity, _observed = liveness_probe

    def stat_file(path: str, mode: str) -> io.BytesIO:
        assert path == "/proc/4242/stat" and mode == "rb"
        return io.BytesIO(raw)

    monkeypatch.setattr(client_module, "open", stat_file, raising=False)
    monkeypatch.setattr(client, "_liveness_process_stat", HerdrClient._liveness_process_stat)
    assert client.process_liveness(identity) == "unknown"


@pytest.mark.parametrize(
    ("flags", "outcome"),
    [(select.POLLIN, "dead"), (select.POLLIN | select.POLLHUP, "dead"),
     (select.POLLERR, "unknown"), (select.POLLNVAL, "unknown"),
     (select.POLLHUP, "unknown"), (select.POLLIN | select.POLLERR, "unknown")],
)
def test_process_liveness_requires_a_valid_pidfd_exit_event(
    liveness_probe: _LivenessFixture, monkeypatch: pytest.MonkeyPatch, flags: int, outcome: str,
) -> None:
    client, identity, _observed = liveness_probe

    class Poller:
        descriptor: int = -1

        def register(self, descriptor: int, events: int) -> None:
            assert events == select.POLLIN
            self.descriptor = descriptor

        def poll(self, timeout: int) -> list[tuple[int, int]]:
            assert timeout == 0
            return [(self.descriptor, flags)]

    monkeypatch.setattr("agentctl.client.select.poll", Poller)
    monkeypatch.setattr(client, "_pidfd_exited", HerdrClient._pidfd_exited)
    assert client.process_liveness(identity) == outcome


@pytest.mark.skipif(not hasattr(os, "pidfd_open"), reason="Linux pidfd identity required")
def test_real_supported_shell_with_background_descendant_is_not_idle() -> None:
    executable = os.path.realpath("/bin/bash")
    shell = subprocess.Popen(
        [executable, "--noprofile", "--norc"], stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        text=True, start_new_session=True,
    )
    try:
        assert shell.stdin is not None
        shell.stdin.write("/usr/bin/sleep 30 & wait\n")
        shell.stdin.flush()
        def child_exists() -> bool:
            for entry in Path("/proc").iterdir():
                if not entry.name.isdigit():
                    continue
                try:
                    parsed = parse_process_stat((entry / "stat").read_bytes())
                except (FileNotFoundError, PermissionError):
                    continue
                if parsed is not None and parsed.ppid == shell.pid:
                    return True
            return False

        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and not child_exists():
            time.sleep(0.01)
        assert child_exists(), "background child did not start"
        process_info: dict[str, object] = {
            "pane_id": "p1",
            "shell_pid": shell.pid,
            "foreground_process_group_id": shell.pid,
            "foreground_processes": [{
                "pid": shell.pid, "name": "bash", "cmdline": executable,
                "argv": [executable], "executable": executable,
            }],
        }
        client = HerdrClient(
            herdr_bin="fixture-herdr", run=Runner({"process_info": process_info})
        )
        identity = client.pane_shell_identity("p1")
        assert not client.pane_is_idle_shell("p1")
        assert not client.pane_is_same_idle_shell("p1", identity)
    finally:
        try:
            os.killpg(shell.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        shell.wait(timeout=5)


def test_recorded_custom_process_identity_survives_atomic_executable_replacement(
    tmp_path: Path,
) -> None:
    executable = tmp_path / "muse"
    replacement = tmp_path / "muse.new"
    shutil.copyfile("/bin/sleep", executable)
    executable.chmod(0o700)
    client = HerdrClient(herdr_bin="fixture-herdr", run=Runner({}))
    first = subprocess.Popen([str(executable), "60"], start_new_session=True)
    second: subprocess.Popen[bytes] | None = None
    try:
        metadata = executable.stat()
        first_info = ProcessInfo(
            pane_id="p1", shell_pid=1, foreground_pgid=first.pid,
            foreground=((first.pid, "muse", str(executable), str(executable), None),),
        )
        identity = client._pane_process_identity(
            first_info, str(executable),
            launch_image=(metadata.st_dev, metadata.st_ino),
        )
        assert isinstance(identity, CustomProcessIdentity)
        assert client._pane_process_identity(first_info, str(executable)) == identity

        shutil.copyfile("/bin/sleep", replacement)
        replacement.chmod(0o700)
        os.replace(replacement, executable)

        assert os.readlink(f"/proc/{first.pid}/exe").endswith(" (deleted)")
        assert client._pane_process_identity(first_info, None, identity) == identity
        assert client._pane_process_identity(first_info, str(executable)) is None

        altered_identities = (
            replace(identity, pid=identity.pid + 1),
            replace(identity, starttime_ticks=identity.starttime_ticks + 1),
            replace(identity, boot_id="ffffffff-ffff-ffff-ffff-ffffffffffff"),
            replace(identity, executable_device=identity.executable_device + 1),
            replace(identity, executable_inode=identity.executable_inode + 1),
        )
        assert all(
            client._pane_process_identity(first_info, None, altered) is None
            for altered in altered_identities
        )
        wrong_group = ProcessInfo(
            pane_id="p1", shell_pid=1, foreground_pgid=first.pid + 1,
            foreground=first_info.foreground,
        )
        assert client._pane_process_identity(wrong_group, None, identity) is None
        duplicated = ProcessInfo(
            pane_id="p1", shell_pid=1, foreground_pgid=first.pid,
            foreground=first_info.foreground * 2,
        )
        assert client._pane_process_identity(duplicated, None, identity) is None

        second = subprocess.Popen([str(executable), "60"], start_new_session=True)
        second_info = ProcessInfo(
            pane_id="p1", shell_pid=1, foreground_pgid=second.pid,
            foreground=((second.pid, "muse", str(executable), str(executable), None),),
        )
        assert client._pane_process_identity(second_info, None, identity) is None
    finally:
        for process in (first, second):
            if process is not None and process.poll() is None:
                process.terminate()
                process.wait(timeout=5)

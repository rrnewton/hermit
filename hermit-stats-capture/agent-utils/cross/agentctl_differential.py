#!/usr/bin/env python3
"""Black-box comparisons of the shared interactive :command:`agentctl` contract.

Each edition runs against its own executable Herdr fixture. Comparisons include
durable identities, queue transitions, human handoff, native goal observation,
and refusal to close an unrelated pane. Optional headless and service adapters
are advertised separately and are not treated as shared capabilities.
"""
from __future__ import annotations

import json
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from typing import cast
from collections.abc import Mapping, Sequence
from pathlib import Path

from herdr_agent_differential import (
    FIXTURE_HERDR, Harness, Outcome, PairCase, Report, _option_values, _queue_snapshot, _revive_interop, _state,
    _stop_recovery_arguments,
    init_case_repository,
)

_COMMON = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
_GOAL_COMMAND = ("--goal-command-json", '["<HERDR>","goal-rpc"]')
_CAPABILITIES = ["send", "status", "read", "wait", "stop", "attach", "pause", "resume",
                 "terminal-snapshot", "drain", "goal", "bind-session"]
_OWNED_CAPABILITIES = [*_CAPABILITIES, "move", "anchor", "rename", "revive"]
_ADOPTED_CAPABILITIES = [*_CAPABILITIES, "anchor", "rename"]
_SHELL_IDENTITY_FIELDS = {
    "version", "boot_id", "pid", "starttime_ticks",
    "executable_device", "executable_inode",
}


def _generated_conversations(value: object) -> set[str]:
    """Only UUIDs selected by the recorded Claude launch, never process boot IDs."""
    if isinstance(value, list):
        return set().union(*(_generated_conversations(item) for item in value))
    if isinstance(value, dict):
        native, arguments = value.get("native_session"), value.get("arguments")
        if (isinstance(native, dict) and native.get("agent") == "claude"
                and isinstance(arguments, list) and len(arguments) >= 2
                and arguments[0] == "--session-id" and arguments[1] == native.get("value")
                and isinstance(arguments[1], str)):
            return {arguments[1]}
    return set()


def _normalize(value: object, conversations: set[str] | None = None) -> object:
    if conversations is None:
        conversations = _generated_conversations(value)
    if isinstance(value, str) and value in conversations:
        return "<CONVERSATION_UUID>"
    if isinstance(value, list):
        return [_normalize(item, conversations) for item in value]
    if isinstance(value, dict):
        return {str(key): ("<TOKEN>" if key == "token" else 0 if key == "created_at"
                          else "<PROCESS_IDENTITY>" if key == "custom_process_identity"
                          # Each edition's fixture runs its own harness process: only its
                          # pid and start time differ, the rest of the identity is compared.
                          else {**item, "pid": "<PID>", "starttime_ticks": "<TICKS>",
                                **({"executable_inode": "<IMAGE_INODE>"}
                                   if value.get("harness") == "muse"
                                   and value.get("adapter") == "herdr-foreign" else {})}
                          if key == "harness_identity" and isinstance(item, dict)
                          else "<HARNESS_PID>" if key == "harness_pid"
                          else "<JOURNAL_ID>" if key == "journal_id"
                          else 0 if key == "renamed_at"
                          else "<ARCHIVE>" if key == "archive"
                          # Native identity diagnostics use edition-specific quote styles.
                          else re.sub(r'"([a-z][a-z0-9-]*)"', r"'\1'", item)
                          if key == "probe_error" and isinstance(item, str) else _normalize(item, conversations))
                for key, item in value.items()}
    return value


def _valid_foreign_shell_identity(value: object) -> bool:
    """Require the complete cross-edition six-field shell identity schema."""
    if not isinstance(value, dict) or set(value) != _SHELL_IDENTITY_FIELDS:
        return False
    boot_id = value.get("boot_id")
    positive_fields = (
        value.get("pid"), value.get("starttime_ticks"),
        value.get("executable_device"), value.get("executable_inode"),
    )
    return (
        value.get("version") == 1
        and isinstance(boot_id, str)
        and re.fullmatch(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            boot_id,
        ) is not None
        and all(isinstance(item, int) and not isinstance(item, bool) and item > 0
                for item in positive_fields)
    )


def _json(outcome: Outcome) -> object:
    try:
        return _normalize(json.loads(outcome.stdout))
    except (TypeError, ValueError):
        return outcome.stdout


def _pair(harness: Harness, report: Report, case: PairCase, label: str,
          arguments: Sequence[str], expected: int = 0) -> tuple[Outcome, Outcome]:
    python, rust = harness.invoke(case, arguments)
    report.require(label, python.returncode == rust.returncode == expected
                   and _json(python) == _json(rust)
                   and (python.stderr == rust.stderr if expected == 0 else True),
                   f"expected rc {expected}; python={python!r} rust={rust!r}")
    if arguments and arguments[0] == "stop" and expected != 0:
        report.require(label + "/recovery-command",
                       all(_stop_recovery_arguments(outcome) is not None for outcome in (python, rust)),
                       f"stop refusal omitted its reason or copyable command: {python!r} {rust!r}")
    return python, rust


def _change(case: PairCase, values: Mapping[str, object]) -> None:
    for root in (case.python_root, case.rust_root):
        state = _state(root)
        state.update(values)
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")


def _submission_count(root: Path, text: str) -> int:
    values = _state(root).get("submitted")
    return values.count(text) if isinstance(values, list) else 0


def _fixture_calls(root: Path) -> list[list[str]]:
    """Read the protocol calls recorded by one executable fixture."""
    values = _state(root).get("calls", [])
    if not isinstance(values, list) or any(
        not isinstance(call, list) or any(not isinstance(word, str) for word in call)
        for call in values
    ):
        raise TypeError("fixture calls must be argv arrays")
    return cast(list[list[str]], values)


def _retire_fixture_processes(case: PairCase) -> None:
    """Stop only custom children created by this differential's private Herdr fixtures."""
    process_ids: list[int] = []
    for root in (case.python_root, case.rust_root):
        state = _state(root)
        for field in ("custom_pid", "retired_custom_pid", "harness_pid"):
            process_id = state.get(field)
            if isinstance(process_id, int) and process_id not in process_ids:
                process_ids.append(process_id)
        subprocess.run(
            [str(root / "fake-herdr"), "pane", "close", "w1:p1"],
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
    deadline = time.monotonic() + 2
    while process_ids and time.monotonic() < deadline:
        remaining: list[int] = []
        for process_id in process_ids:
            try:
                process_stat = Path(f"/proc/{process_id}/stat").read_text(encoding="utf-8")
            except (FileNotFoundError, ProcessLookupError):
                continue
            close = process_stat.rfind(")")
            if close < 0 or process_stat[close + 2:].split(maxsplit=1)[0] != "Z":
                remaining.append(process_id)
        process_ids = remaining
        if process_ids:
            time.sleep(0.01)
    if process_ids:
        raise AssertionError(f"fixture custom processes did not exit: {process_ids!r}")


def _start(harness: Harness, report: Report, case: PairCase, label: str,
           *options: str) -> bool:
    outcomes = _pair(harness, report, case, label,
                     ("start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *options, *_COMMON))
    return all(outcome.returncode == 0 for outcome in outcomes)


def _orientation(harness: Harness, report: Report) -> None:
    case = harness.case("primary-orientation")
    _pair(harness, report, case, "primary/version", ("--version",))
    for arguments in ((), ("--help",), ("start", "--help"), ("adopt", "--help"),
                      ("stop", "--help"), ("send", "--help"), ("goal", "--help")):
        for edition, outcome in zip(("python", "rust"), harness.invoke(case, arguments), strict=True):
            report.require(f"primary/help/{arguments}/{edition}",
                           outcome.returncode == 0 and not outcome.stderr and "--registry" in outcome.stdout
                           and ("--message-id" in outcome.stdout if arguments == ("send", "--help") else True)
                           and ("--env" in outcome.stdout if arguments == ("start", "--help") else True)
                           and (all(option in outcome.stdout for option in
                                    ("--pane", "--workspace", "--cwd", "--harness", "--session"))
                                and "muse requires a pinned foreground process" in
                                " ".join(outcome.stdout.lower().split())
                                if arguments == ("adopt", "--help") else True)
                           and (all(option in outcome.stdout for option in (
                               "--recover-legacy-adoption", "--retire-dead-adoption", "--expected-token",
                               "--expected-record-sha256",
                           )) if arguments == ("stop", "--help") else True),
                           f"missing operation help: {outcome!r}")
    for command in ("quickstart", "userguide"):
        for edition, outcome in zip(("python", "rust"), harness.invoke(case, (command,)), strict=True):
            report.require(f"primary/{command}/{edition}", outcome.returncode == 0 and not outcome.stderr
                           and "agentctl start" in outcome.stdout and "agentctl send" in outcome.stdout,
                           f"installed guide has no working lifecycle: {outcome!r}")
    for edition, outcome in zip(("python", "rust"), harness.invoke(case, ("capabilities",)), strict=True):
        value = _json(outcome)
        report.require(f"primary/capabilities/{edition}", outcome.returncode == 0 and isinstance(value, dict)
                       and value.get("interactive") == {"backends": ["herdr"], "harnesses": ["codex", "claude", "muse"]}
                       and value.get("registry") == ".agentctl",
                       f"shared adapter metadata differs: {outcome!r}")
    _pair(harness, report, case, "primary/empty-list", ("list", *_COMMON))
    report.require("primary/empty-list-observational",
                   all(not (root / "registry").exists() for root in (case.python_root, case.rust_root)),
                   "listing an absent registry created persistent state")


def _lifecycle(harness: Harness, report: Report) -> None:
    for kind in ("codex", "claude"):
        case = harness.case(f"primary-{kind}")
        if not _start(harness, report, case, f"primary/{kind}/start", "--harness", kind,
                      "--model", "chosen-model", "--resume", "session-1", "--harness-arg=--extra",
                      "--env", "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
                      "--env", "LITERAL= spaces $(unexpanded) = remain "):
            continue
        for command in (("status", "worker"), ("list",), ("wait", "worker", "--timeout", "0")):
            _pair(harness, report, case, f"primary/{kind}/{command[0]}", (*command, *_COMMON))
        expected_args = (["resume", "session-1", "--no-alt-screen"] if kind == "codex"
                         else ["--resume", "session-1"]) + ["--model", "chosen-model", "--extra"]
        report.require(f"primary/{kind}/native-launch-presets",
                       all(_state(root).get("launch_arguments") == expected_args
                           for root in (case.python_root, case.rust_root)), "native launch argv was changed or split")
        expected_environment = [
            "META_CODEX_AI_GATEWAY=azure-codex-cyber:openai",
            "LITERAL= spaces $(unexpanded) = remain ",
        ]
        report.require(f"primary/{kind}/literal-tab-environment",
                       all(_state(root).get("tab_environment") == expected_environment
                           for root in (case.python_root, case.rust_root)),
                       "tab environment was changed, split, or reordered")
        python, _ = _pair(harness, report, case, f"primary/{kind}/metadata", ("status", "worker", *_COMMON))
        status = _json(python)
        report.require(f"primary/{kind}/metadata-contract", isinstance(status, dict)
                       and all(status.get(key) == value for key, value in {
                           "name": "worker", "adapter": "herdr", "mode": "interactive", "backend": "herdr",
                           "paused": False, "runtime_home": None,
                           "capabilities": _OWNED_CAPABILITIES,
                           "pane_id": "w1:p1", "session_value": "session-1", "lifecycle": "running",
                       }.items()), f"missing canonical identity: {status!r}")
        report.require(f"primary/{kind}/environment-not-in-status",
                       isinstance(status, dict)
                       and all(entry not in json.dumps(status) for entry in expected_environment),
                       f"status exposed launch environment values: {status!r}")
        _pair(harness, report, case, f"primary/{kind}/bind", ("bind-session", "worker", "session-1", *_GOAL_COMMAND, *_COMMON))
        _pair(harness, report, case, f"primary/{kind}/goal-absent", ("goal", "worker", *_COMMON))
        python, _ = _pair(harness, report, case, f"primary/{kind}/set-goal", ("goal", "worker", "finish review", *_COMMON))
        goal = _json(python)
        report.require(f"primary/{kind}/goal-evidence", isinstance(goal, dict)
                       and goal.get("goal") == "finish review" and goal.get("delivery") == "delivered"
                       and goal.get("source") == ("native" if kind == "codex" else "requested")
                       and goal.get("native_status") == ("active" if kind == "codex" else "unverified"),
                       f"requested and native goal state were conflated: {goal!r}")
        _pair(harness, report, case, f"primary/{kind}/read-goal", ("goal", "worker", *_COMMON))
        text = "literal 'quotes' and $(unexpanded)\nsecond line"
        _pair(harness, report, case, f"primary/{kind}/send", ("send", "worker", text, "--message-id", "followup", *_COMMON))
        _pair(harness, report, case, f"primary/{kind}/duplicate-id", ("send", "worker", text, "--message-id", "followup", *_COMMON), 75)
        report.require(f"primary/{kind}/send-exactly-once", all(_submission_count(root, text) == 1
                       for root in (case.python_root, case.rust_root)), "explicit ID was delivered twice")
        for root in (case.python_root, case.rust_root):
            (root / "prompt.txt").write_text("file instruction\nwith newline\n", encoding="utf-8")
        _pair(harness, report, case, f"primary/{kind}/send-file", ("send", "worker", "--file", "<ROOT>/prompt.txt", *_COMMON))
        _pair(harness, report, case, f"primary/{kind}/read", ("read", "worker", "--lines", "17", *_COMMON))
        report.require(f"primary/{kind}/read-snapshot", all((root / "registry/worker/output.json").is_file()
                       for root in (case.python_root, case.rust_root)), "read did not retain its documented terminal snapshot")
        _pair(harness, report, case, f"primary/{kind}/stop", ("stop", "worker", *_COMMON))
        _pair(harness, report, case, f"primary/{kind}/list-stopped", ("list", *_COMMON))
        report.require(f"primary/{kind}/exact-pane-retirement", all(
            _state(root).get("closed_panes") == ["w1:p1"] and not (root / "registry/worker").exists()
            and len(list((root / "registry/archive").iterdir())) == 1
                       for root in (case.python_root, case.rust_root)), "owned pane or archival contract diverged")

    custom = harness.case("primary-existing-native-kind")
    python, _ = _pair(harness, report, custom, "primary/native-kind/start", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--harness", "third-party", *_COMMON,
    ))
    status = _json(python)
    report.require("primary/native-kind/adapter",
                   isinstance(status, dict) and status.get("adapter") == "herdr"
                   and all(_state(root).get("harness") == "third-party"
                           and _state(root).get("custom_harness") is not True
                           for root in (custom.python_root, custom.rust_root)),
                   f"existing Herdr-native third-party kind was rerouted: {status!r}")
    _pair(harness, report, custom, "primary/native-kind/stop", (
        "stop", "worker", *_COMMON,
    ))


def _conversations(harness: Harness, report: Report) -> None:
    case = harness.case("primary-claude-conversation")
    _change(case, {"sessionless": True})
    outcomes = _pair(harness, report, case, "primary/conversation/claude/start", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", "--harness", "claude", *_COMMON,
    ))
    for edition, root, outcome in zip(("python", "rust"), (case.python_root, case.rust_root), outcomes, strict=True):
        if outcome.returncode:
            continue
        value = json.loads(outcome.stdout)
        native = value.get("native_session")
        arguments = _state(root).get("launch_arguments")
        report.require(f"primary/conversation/claude/provenance/{edition}",
                       isinstance(native, dict) and set(native) == {"schema", "agent", "value", "source"}
                       and native.get("schema") == "agentctl-native-session/v1"
                       and native.get("agent") == "claude" and native.get("source") == "asserted"
                       and isinstance(native.get("value"), str)
                       and re.fullmatch(r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}", native["value"]) is not None
                       and arguments == ["--session-id", native["value"]]
                       and value.get("session_value") is None,
                       f"generated conversation not retained separately from routing identity: {value!r}")
    _pair(harness, report, case, "primary/conversation/claude/status", ("status", "worker", *_COMMON))
    _pair(harness, report, case, "primary/conversation/claude/list", ("list", *_COMMON))
    _pair(harness, report, case, "primary/conversation/claude/stop", ("stop", "worker", *_COMMON))
    for selector in ("--session-id=foreign", "--continue", "-rforeign", "-cforeign", "--fork-session"):
        rejected = harness.case(f"primary-claude-conversation-conflict-{selector}")
        _pair(harness, report, rejected, f"primary/conversation/claude/refuse-{selector}", (
            "start", "worker", "--cwd", "<ROOT>", "--harness", "claude", f"--harness-arg={selector}", *_COMMON,
        ), 75)
        report.require(f"primary/conversation/claude/no-allocation-{selector}",
                       all(not (root / "registry/worker").exists() for root in (rejected.python_root, rejected.rust_root)),
                       "conflicting conversation allocated a runtime")
    forked = harness.case("primary-claude-resume-fork")
    _pair(harness, report, forked, "primary/conversation/claude/refuse-resume-fork", (
        "start", "worker", "--cwd", "<ROOT>", "--harness", "claude", "--resume", "chosen-conversation",
        "--harness-arg=--fork-session", *_COMMON,
    ), 75)
    report.require("primary/conversation/claude/no-allocation-resume-fork",
                   all(not (root / "registry/worker").exists() for root in (forked.python_root, forked.rust_root)),
                   "fork selector allocated a replacement conversation")
    mismatch = harness.case("primary-resume-conversation-mismatch")
    _change(mismatch, {"session_value_override": "wrong-conversation"})
    _pair(harness, report, mismatch, "primary/conversation/resume/mismatch", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", "--harness", "codex",
        "--resume", "chosen-conversation", "--brief", "must never arrive", *_COMMON,
    ), 75)
    report.require("primary/conversation/resume/no-brief",
                   all(not _state(root).get("submitted") for root in (mismatch.python_root, mismatch.rust_root)),
                   "brief reached a contradictory conversation")


def _profiles(harness: Harness, report: Report) -> None:
    document = {
        "schema": "agentctl-profiles/v1",
        "profiles": {
            "astra-ultra": {
                "harness": "codex", "mode": "interactive", "model": "gpt-6-astra",
                "reasoning_effort": "ultra",
                "argv": ["--dangerously-enable-internet-mode", "--dangerously-bypass-approvals-and-sandbox"],
                "env": {"META_CODEX_AI_GATEWAY": "azure-codex-cyber:openai"},
            },
            "sol": {
                "harness": "codex", "mode": "interactive", "model": "gpt-5.6-sol",
                "argv": ["--dangerously-enable-internet-mode", "--dangerously-bypass-approvals-and-sandbox"],
                "env": {"META_CODEX_AI_GATEWAY": "azure-codex-cyber:openai"},
            },
            "watermelon": {
                "harness": "muse", "mode": "interactive",
                "model": "kiki_gb300_mxfp8_6p2_840_nwr", "reasoning_effort": "ultra",
                "argv": [], "env": {},
            },
            "watermelon-exec": {
                "harness": "muse", "mode": "headless",
                "model": "kiki_gb300_mxfp8_6p2_840_nwr", "reasoning_effort": "ultra",
                "argv": ["--image=private-fixture.png"], "env": {},
            },
            "muse-literal": {
                "harness": "muse", "mode": "interactive",
                "argv": [
                    "--trust-workspace",
                    '--meta-tag=literal $(unexpanded) "quotes"',
                    "--meta-tag=repeatable",
                ],
                "env": {},
            },
            "codex-safe-config": {
                "harness": "codex", "mode": "interactive",
                "argv": ["--config=features.web_search=true"], "env": {},
            },
            # An explicit null agentcloud block means absent in both editions.
            "codex-null-cloud": {
                "harness": "codex", "mode": "interactive", "agentcloud": None,
            },
            # Launchable only by the Rust edition; both editions must list it identically and
            # its presence must not break any other profile in the file.
            "agentcloud-worker": {
                "harness": "agentcloud", "mode": "interactive", "model": "provider-model",
                "argv": ["--narration"], "env": {"TAB_SETTING": "private-tab-value"},
                "agentcloud": {"harness": "claude-code", "provision": True,
                               "envspec": "<envspec>", "purpose": "<purpose>",
                               "title": "{name}-{host}"},
            },
        },
    }
    expected = {
        "astra-ultra": ["--no-alt-screen", "--model", "gpt-6-astra", "--config",
                         "model_reasoning_effort=ultra", "--dangerously-enable-internet-mode",
                         "--dangerously-bypass-approvals-and-sandbox"],
        "sol": ["--no-alt-screen", "--model", "gpt-5.6-sol",
                "--dangerously-enable-internet-mode", "--dangerously-bypass-approvals-and-sandbox"],
        "watermelon": ["--model", "kiki_gb300_mxfp8_6p2_840_nwr", "--reasoning-effort", "ultra"],
        "muse-literal": ["--trust-workspace", '--meta-tag=literal $(unexpanded) "quotes"',
                         "--meta-tag=repeatable"],
        "codex-safe-config": ["--no-alt-screen", "--config=features.web_search=true"],
    }
    for profile in ("astra-ultra", "sol", "watermelon", "muse-literal", "codex-safe-config"):
        case = harness.case(f"primary-profile-{profile}")
        for root in (case.python_root, case.rust_root):
            init_case_repository(root)
            (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            directory = root / ".agentctl"
            directory.mkdir(mode=0o700)
            config = directory / "profiles.json"
            config.write_text(json.dumps(document), encoding="utf-8")
            config.chmod(0o600)
        listed = _pair(harness, report, case, f"primary/profile/{profile}/list",
                       ("profiles", "--cwd", "<ROOT>"))
        for edition, outcome in zip(("python", "rust"), listed, strict=True):
            public = json.loads(outcome.stdout)
            serialized = json.dumps(public, sort_keys=True)
            report.require(
                f"primary/profile/{profile}/list-redaction/{edition}",
                "azure-codex-cyber:openai" not in serialized
                and "--dangerously-enable-internet-mode" not in serialized
                and "private-fixture.png" not in serialized
                and "private-tab-value" not in serialized
                and any(
                    item.get("name") == "codex-null-cloud" and "agentcloud" not in item
                    for item in public.get("profiles", [])
                )
                and any(
                    item.get("name") == "agentcloud-worker"
                    and item.get("requires_edition") == "rust"
                    and item.get("argv_count") == 1
                    and item.get("agentcloud") == {
                        "harness": "claude-code", "provision": True, "envspec": "<envspec>",
                        "purpose": "<purpose>", "workspace": None, "node_id": None,
                        "title": "{name}-{host}",
                    }
                    for item in public.get("profiles", [])
                )
                and any(
                    item.get("name") == "watermelon-exec"
                    and item.get("harness") == "muse"
                    and item.get("mode") == "headless"
                    and item.get("argv_count") == 1
                    and item.get("environment") == []
                    for item in public.get("profiles", [])
                ),
                f"safe profile metadata disclosed private values or omitted headless Muse: {public!r}",
            )
        python, rust = _pair(harness, report, case, f"primary/profile/{profile}/start", (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--profile", profile, *_COMMON,
        ))
        status = _json(python)
        report.require(f"primary/profile/{profile}/recorded-policy", isinstance(status, dict)
                       and status.get("profile") == profile,
                       f"launch profile was not retained: {status!r}")
        report.require(f"primary/profile/{profile}/argv", all(
            _state(root).get("launch_arguments") == expected[profile]
            for root in (case.python_root, case.rust_root)
        ), "profile argv changed, split, or reordered")
        if profile == "sol":
            # The Rust start would contact a real agentcloud service, so only the Python
            # refusal is exercised here; the Rust launcher is covered by its own tests.
            python_refusal = harness._invoke_one(harness.python, case.python_root, (
                "start", "sub-cloud", "--cwd", "<ROOT>", "--workspace-id", "w1",
                "--profile", "agentcloud-worker", *_COMMON,
            ))
            report.require(
                "primary/profile/agentcloud/python-start-refusal",
                python_refusal.returncode == 2
                and "only the Rust edition of agentctl implements" in python_refusal.stderr
                and not (case.python_root / "registry" / "sub-cloud").exists(),
                f"Python started or mis-refused a Rust-only agentcloud profile: {python_refusal!r}",
            )
        adapter = "herdr-pane" if profile in ("watermelon", "muse-literal") else "herdr"
        report.require(f"primary/profile/{profile}/adapter",
                       isinstance(status, dict) and status.get("adapter") == adapter,
                       f"profile selected wrong adapter: {status!r}")
        if profile == "watermelon":
            for edition, outcome in zip(("python", "rust"), (python, rust), strict=True):
                raw_status = json.loads(outcome.stdout)
                identity = raw_status.get("custom_process_identity")
                report.require(
                    f"primary/profile/watermelon/process-identity/{edition}",
                    isinstance(identity, dict)
                    and set(identity) == {
                        "version", "boot_id", "pid", "starttime_ticks",
                        "executable_device", "executable_inode",
                    }
                    and identity.get("version") == 1
                    and isinstance(identity.get("boot_id"), str)
                    and all(
                        isinstance(identity.get(field), int) and identity[field] > 0
                        for field in (
                            "pid", "starttime_ticks", "executable_device", "executable_inode",
                        )
                    ),
                    f"{edition} omitted the pinned custom process identity: {raw_status!r}",
                )
            report.require("primary/profile/watermelon/effective-effort",
                           isinstance(status, dict)
                           and status.get("effective_reasoning_effort") == "xhigh"
                           and status.get("startup_warning") == (
                               "reasoning effort ultra is not available "
                               "(gate ultra_reasoning_effort is closed); using xhigh"
                           ), f"Muse downgrade was hidden or misreported: {status!r}")
            for root in (case.python_root, case.rust_root):
                executable = root / "fake-muse-runtime"
                replacement = root / "fake-muse-runtime.new"
                shutil.copyfile(sys.executable, replacement)
                replacement.chmod(0o700)
                os.replace(replacement, executable)
                process_id = _state(root).get("custom_pid")
                report.require(
                    f"primary/profile/watermelon/deleted-image/{root.parent.name}",
                    isinstance(process_id, int)
                    and os.readlink(f"/proc/{process_id}/exe").endswith(" (deleted)"),
                    "atomic replacement did not retain the old running executable inode",
                )
            _pair(harness, report, case, "primary/profile/watermelon/status-after-upgrade", (
                "status", "worker", *_COMMON,
            ))
            original_records = {
                root: json.loads(
                    (root / "registry/worker/agent.json").read_text(encoding="utf-8")
                )
                for root in (case.python_root, case.rust_root)
            }
            mismatch_values: tuple[tuple[str, str, object], ...] = (
                ("pid", "pid", 1),
                ("starttime", "starttime_ticks", 1),
                ("boot", "boot_id", "ffffffff-ffff-ffff-ffff-ffffffffffff"),
                ("device", "executable_device", 1),
                ("inode", "executable_inode", 1),
            )
            for label, field, adjustment in mismatch_values:
                for root, original in original_records.items():
                    changed = json.loads(json.dumps(original))
                    if field == "boot_id":
                        changed["custom_process_identity"][field] = adjustment
                    else:
                        changed["custom_process_identity"][field] += adjustment
                    (root / "registry/worker/agent.json").write_text(
                        json.dumps(changed), encoding="utf-8"
                    )
                _pair(
                    harness, report, case,
                    f"primary/profile/watermelon/refuse-{label}-status",
                    ("status", "worker", *_COMMON),
                )
                refused = harness.invoke(case, ("stop", "worker", *_COMMON))
                report.require(
                    f"primary/profile/watermelon/refuse-{label}-stop",
                    all(outcome.returncode == 69 for outcome in refused)
                    and all(
                        (root / "registry/worker/agent.json").is_file()
                        and not _state(root).get("closed")
                        for root in (case.python_root, case.rust_root)
                    ),
                    f"{label} identity mismatch allowed pane retirement: {refused!r}",
                )
                for root, original in original_records.items():
                    (root / "registry/worker/agent.json").write_text(
                        json.dumps(original), encoding="utf-8"
                    )
            _change(case, {"wrong_custom_process_group": True})
            _pair(
                harness, report, case,
                "primary/profile/watermelon/refuse-process-group-status",
                ("status", "worker", *_COMMON),
            )
            refused = harness.invoke(case, ("stop", "worker", *_COMMON))
            report.require(
                "primary/profile/watermelon/refuse-process-group-stop",
                all(outcome.returncode == 69 for outcome in refused)
                and all(
                    (root / "registry/worker/agent.json").is_file()
                    and not _state(root).get("closed")
                    for root in (case.python_root, case.rust_root)
                ),
                f"foreground process-group mismatch allowed pane retirement: {refused!r}",
            )
            _change(case, {"wrong_custom_process_group": False})

            malformed_variants: tuple[tuple[str, str, object], ...] = (
                ("bool", "pid", True),
                ("overflow", "starttime_ticks", 1 << 64),
                ("zero-device", "executable_device", 0),
                ("unknown", "unexpected", 1),
            )
            for label, field, value in malformed_variants:
                for root, original in original_records.items():
                    changed = json.loads(json.dumps(original))
                    changed["custom_process_identity"][field] = value
                    (root / "registry/worker/agent.json").write_text(
                        json.dumps(changed), encoding="utf-8"
                    )
                _pair(
                    harness, report, case,
                    f"primary/profile/watermelon/malformed-{label}",
                    ("status", "worker", *_COMMON), 75,
                )
                for root, original in original_records.items():
                    (root / "registry/worker/agent.json").write_text(
                        json.dumps(original), encoding="utf-8"
                    )
            prompt = "Muse literal $(unexpanded) delivery\nsecond line"
            _pair(harness, report, case, "primary/profile/watermelon/send", (
                "send", "worker", prompt, "--message-id", "muse-first", *_COMMON,
            ))
            report.require("primary/profile/watermelon/pane-delivery",
                           all(_state(root).get("submitted") == [prompt]
                               and _state(root).get("paste_wrapped") is True
                               and (root / "registry/worker/queue/processed/muse-first.json").is_file()
                               and not list((root / "registry/worker/queue/inflight").glob("*.json"))
                               and not list((root / "registry/worker/queue/failed").glob("*.json"))
                               for root in (case.python_root, case.rust_root)),
                           "Muse pane delivery lacked an exact post-Enter receipt")
            _change(case, {
                "custom_post_error": True, "custom_submitted": False, "custom_draft": "",
            })
            _pair(harness, report, case, "primary/profile/watermelon/redraw-refusal", (
                "send", "worker", "must remain uncertain", "--message-id", "muse-error",
                "--working-timeout", "0.05", *_COMMON,
            ), 76)
            report.require("primary/profile/watermelon/redraw-quarantine", all(
                (root / "registry/worker/queue/failed/muse-error.json").is_file()
                for root in (case.python_root, case.rust_root)
            ), "Muse draft/error redraw was incorrectly accepted as delivery")
            _change(case, {
                "custom_post_error": False, "custom_clear_without_submit": True,
                "custom_submitted": False, "custom_draft": "",
            })
            _pair(harness, report, case, "primary/profile/watermelon/repeated-redraw", (
                "send", "worker", prompt, "--message-id", "muse-repeat",
                "--working-timeout", "0.05", *_COMMON,
            ), 76)
            report.require("primary/profile/watermelon/repeated-quarantine", all(
                (root / "registry/worker/queue/failed/muse-repeat.json").is_file()
                for root in (case.python_root, case.rust_root)
            ), "an old identical transcript entry was mistaken for a new submission")
        if profile not in ("watermelon", "muse-literal"):
            expected_environment = (
                ["META_CODEX_AI_GATEWAY=azure-codex-cyber:openai"]
                if profile in ("astra-ultra", "sol") else []
            )
            report.require(f"primary/profile/{profile}/environment", all(
                _state(root).get("tab_environment") == expected_environment
                for root in (case.python_root, case.rust_root)
            ), "profile environment changed or leaked")
        _pair(harness, report, case, f"primary/profile/{profile}/stop", (
            "stop", "worker", *_COMMON,
        ))


def _profile_resume(harness: Harness, report: Report) -> None:
    """A profile resumes the asserted conversation without changing its launch policy."""
    conversation = "11111111-1111-4111-8111-111111111111"
    literal_argument = '--meta-tag=literal $(unexpanded) "quotes"'
    literal_environment = 'PROFILE_SELECTOR=literal $(unexpanded) "quotes"'
    expected_arguments = {
        "claude": ["--resume", conversation, "--model", "selected-model", "--effort", "high", literal_argument],
        "codex": ["resume", conversation, "--no-alt-screen", "--model", "selected-model", "--config",
                  "model_reasoning_effort=high", literal_argument],
        "muse": ["--model", "selected-model", "--reasoning-effort", "high", literal_argument,
                 "resume", conversation],
    }

    def configured_case(kind: str, scenario: str, *, raw: Sequence[str] | None = None) -> PairCase:
        case = harness.case(f"primary-profile-resume-{kind}-{scenario}", {
            "report_custom_session": True,
            **({"session_value_override": "wrong-conversation"} if scenario == "mismatch" else {}),
        })
        document = {
            "schema": "agentctl-profiles/v1",
            "profiles": {"local-policy": {
                "harness": kind, "mode": "interactive", "model": "selected-model",
                "reasoning_effort": "high", "argv": [literal_argument] if raw is None else list(raw),
                "env": {"PROFILE_SELECTOR": literal_environment.partition("=")[2]},
            }},
        }
        for root in (case.python_root, case.rust_root):
            init_case_repository(root)
            (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            directory = root / ".agentctl"
            directory.mkdir(mode=0o700)
            config = directory / "profiles.json"
            config.write_text(json.dumps(document), encoding="utf-8")
            config.chmod(0o600)
        return case

    for kind in ("claude", "codex", "muse"):
        selected = configured_case(kind, "selected")
        outcomes = _pair(harness, report, selected, f"primary/profile/resume/{kind}/start", (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", "--profile", "local-policy",
            "--resume", conversation, "--brief", "continue the saved conversation", *_COMMON,
        ))
        for edition, root, outcome in zip(
            ("python", "rust"), (selected.python_root, selected.rust_root), outcomes, strict=True,
        ):
            if outcome.returncode:
                continue
            value = _json(outcome)
            native = {
                "schema": "agentctl-native-session/v1", "agent": kind,
                "value": conversation, "source": "asserted",
            }
            report.require(f"primary/profile/resume/{kind}/policy/{edition}",
                           isinstance(value, dict) and value.get("profile") == "local-policy"
                           and value.get("model") == "selected-model"
                           and value.get("reasoning_effort") == "high"
                           and value.get("cwd") == "<ROOT>" and value.get("mode") == "interactive"
                           and value.get("arguments") == expected_arguments[kind]
                           and value.get("native_session") == native and value.get("resume") == conversation
                           and value.get("session_agent") == kind and value.get("session_value") == conversation
                           and value.get("environment_names") == ["PROFILE_SELECTOR"],
                           f"profile resume did not retain its conversation and policy: {value!r}")
            state = _state(root)
            report.require(f"primary/profile/resume/{kind}/launch/{edition}",
                           state.get("launch_arguments") == expected_arguments[kind]
                           and state.get("tab_environment") == [literal_environment]
                           and state.get("submitted") == ["continue the saved conversation"],
                           "the literal launch policy or startup brief changed")
        _pair(harness, report, selected, f"primary/profile/resume/{kind}/status", (
            "status", "worker", *_COMMON,
        ))
        _pair(harness, report, selected, f"primary/profile/resume/{kind}/list", ("list", *_COMMON))
        roots = (selected.python_root, selected.rust_root)
        before = {
            root: ((_state(root).get("calls")), (root / "registry/worker/agent.json").read_bytes())
            for root in roots if (root / "registry/worker/agent.json").exists()
        }
        _pair(harness, report, selected, f"primary/profile/resume/{kind}/occupied", (
            "start", "other-worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--profile", "local-policy", "--resume", conversation, *_COMMON,
        ), 75)
        report.require(f"primary/profile/resume/{kind}/occupied-atomic",
                       len(before) == 2 and all(
                           not (root / "registry/other-worker").exists()
                           and _state(root).get("calls") == calls
                           and (root / "registry/worker/agent.json").read_bytes() == record
                           for root, (calls, record) in before.items()
                       ), "an occupied conversation changed its owner or allocated a new tab")
        _pair(harness, report, selected, f"primary/profile/resume/{kind}/stop", (
            "stop", "worker", *_COMMON,
        ))

        mismatch = configured_case(kind, "mismatch")
        _pair(harness, report, mismatch, f"primary/profile/resume/{kind}/mismatch", (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--profile", "local-policy", "--resume", conversation, "--brief", "must never arrive", *_COMMON,
        ), 75)
        report.require(f"primary/profile/resume/{kind}/mismatch-no-brief",
                       all(not _state(root).get("submitted")
                           for root in (mismatch.python_root, mismatch.rust_root)),
                       "a startup brief reached a contradictory conversation")
        _retire_fixture_processes(mismatch)

        raw_selector = ("--resume", "other-conversation") if kind == "claude" else ("resume", "other-conversation")
        duplicate = configured_case(kind, "duplicate", raw=raw_selector)
        _pair(harness, report, duplicate, f"primary/profile/resume/{kind}/duplicate", (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--profile", "local-policy", "--resume", conversation, *_COMMON,
        ), 75)
        report.require(f"primary/profile/resume/{kind}/duplicate-atomic",
                       all(not (root / "registry").exists() and not _state(root).get("calls")
                           for root in (duplicate.python_root, duplicate.rust_root)),
                       "duplicate resume selectors allocated a registry or contacted Herdr")


def _skill_install(harness: Harness, report: Report) -> None:
    case = harness.case("primary-skill-install")
    first_python, first_rust = harness.invoke(case, ("skill", "install"))
    report.require("primary/skill/install", first_python.returncode == first_rust.returncode == 0
                   and _json(first_python) == _json(first_rust),
                   f"skill install diverged: {first_python!r} {first_rust!r}")
    for root in (case.python_root, case.rust_root):
        paths = [
            root / "skill-homes/codex/agentctl/SKILL.md",
            root / "skill-homes/claude/agentctl/SKILL.md",
            root / "xdg-config/muse/skills/agentctl/SKILL.md",
        ]
        report.require(f"primary/skill/content/{root.parent.name}",
                       all(path.is_file() for path in paths)
                       and len({path.read_bytes() for path in paths}) == 1,
                       "not every harness received the same skill")
        calls = (root / "muse-skill-calls.jsonl").read_text(encoding="utf-8").splitlines()
        report.require(f"primary/skill/muse-native/{root.parent.name}", len(calls) == 1,
                       "Muse skill did not use its native managed installer exactly once")
    second_python, second_rust = harness.invoke(case, ("skill", "install"))
    report.require("primary/skill/idempotent", second_python.returncode == second_rust.returncode == 0
                   and _json(second_python) == _json(second_rust)
                   and _json(second_python) == {"installed": [], "unchanged": ["codex", "claude", "muse"]},
                   f"second install was not idempotent: {second_python!r} {second_rust!r}")
    report.require("primary/skill/muse-idempotent-native", all(
        len((root / "muse-skill-calls.jsonl").read_text(encoding="utf-8").splitlines()) == 1
        for root in (case.python_root, case.rust_root)
    ), "idempotent Muse install needlessly invoked the native installer")
    for root in (case.python_root, case.rust_root):
        (root / "skill-homes/codex/agentctl/SKILL.md").write_text("owner customization\n", encoding="utf-8")
    refused = harness.invoke(case, ("skill", "install", "--harness", "codex"))
    report.require("primary/skill/divergent-refusal",
                   all(outcome.returncode == 75 for outcome in refused),
                   f"divergent skill was overwritten without force: {refused!r}")
    forced_python, forced_rust = harness.invoke(
        case, ("skill", "install", "--harness", "codex", "--force")
    )
    report.require("primary/skill/explicit-force",
                   forced_python.returncode == forced_rust.returncode == 0
                   and _json(forced_python) == _json(forced_rust),
                   f"explicit skill replacement diverged: {forced_python!r} {forced_rust!r}")

    for root in (case.python_root, case.rust_root):
        (root / "xdg-config/muse/skills/agentctl/SKILL.md").write_text(
            "owner customization\n", encoding="utf-8"
        )
    refused = harness.invoke(case, ("skill", "install", "--harness", "muse"))
    report.require("primary/skill/muse-divergent-refusal",
                   all(outcome.returncode == 75 for outcome in refused)
                   and all(
                       (root / "xdg-config/muse/skills/agentctl/SKILL.md").read_text(
                           encoding="utf-8"
                       ) == "owner customization\n"
                       for root in (case.python_root, case.rust_root)
                   ), f"divergent managed Muse skill was overwritten: {refused!r}")
    forced_python, forced_rust = harness.invoke(
        case, ("skill", "install", "--harness", "muse", "--force")
    )
    report.require("primary/skill/muse-explicit-force",
                   forced_python.returncode == forced_rust.returncode == 0
                   and _json(forced_python) == _json(forced_rust)
                   and all(
                       len((root / "muse-skill-calls.jsonl").read_text(
                           encoding="utf-8"
                       ).splitlines()) == 2
                       for root in (case.python_root, case.rust_root)
                   ), f"explicit managed Muse replacement diverged: {forced_python!r} {forced_rust!r}")

    symlink = harness.case("primary-skill-symlink")
    for root in (symlink.python_root, symlink.rust_root):
        destination = root / "skill-homes/codex/agentctl"
        destination.mkdir(parents=True, mode=0o700)
        (root / "outside-skill").write_text("outside\n", encoding="utf-8")
        (destination / "SKILL.md").symlink_to(root / "outside-skill")
    outcomes = harness.invoke(symlink, ("skill", "install", "--harness", "codex", "--force"))
    report.require("primary/skill/symlink-refusal",
                   all(outcome.returncode == 75 for outcome in outcomes)
                   and all((root / "outside-skill").read_text(encoding="utf-8") == "outside\n"
                           for root in (symlink.python_root, symlink.rust_root)),
                   f"skill symlink was followed: {outcomes!r}")

    muse_symlink = harness.case("primary-skill-muse-symlink")
    for root in (muse_symlink.python_root, muse_symlink.rust_root):
        outside = root / "outside-muse-skills"
        outside.mkdir(mode=0o700)
        destination = root / "xdg-config/muse"
        destination.mkdir(parents=True, mode=0o700)
        (destination / "skills").symlink_to(outside)
    outcomes = harness.invoke(
        muse_symlink, ("skill", "install", "--harness", "muse", "--force")
    )
    report.require("primary/skill/muse-symlink-refusal",
                   all(outcome.returncode == 75 for outcome in outcomes)
                   and all(not list((root / "outside-muse-skills").iterdir())
                           for root in (muse_symlink.python_root, muse_symlink.rust_root)),
                   f"Muse skill destination symlink was followed: {outcomes!r}")

    missing_muse = harness.case("primary-skill-muse-missing-executable")
    for root in (missing_muse.python_root, missing_muse.rust_root):
        (root / "fake-muse-skills").unlink()
    outcomes = harness.invoke(missing_muse, ("skill", "install", "--harness", "muse"))
    report.require("primary/skill/muse-missing-executable",
                   all(outcome.returncode == 75 for outcome in outcomes)
                   and all(not (root / "xdg-config/muse/skills").exists()
                           for root in (missing_muse.python_root, missing_muse.rust_root)),
                   f"missing Muse installer mutated configuration: {outcomes!r}")

    for variable, selected in (
        ("AGENTCTL_CODEX_SKILLS_DIR", "codex"),
        ("AGENTCTL_CLAUDE_SKILLS_DIR", "claude"),
        ("AGENTCTL_MUSE_BIN", "muse"),
        ("XDG_CONFIG_HOME", "muse"),
    ):
        empty = harness.case(
            f"primary-skill-empty-{variable.lower()}",
            {"empty_environment": [variable]},
        )
        outcomes = harness.invoke(empty, (
            "skill", "install", "--harness", selected,
        ))
        report.require(f"primary/skill/empty/{variable}",
                       all(outcome.returncode == 75 for outcome in outcomes),
                       f"empty environment override diverged or was accepted: {outcomes!r}")


def _profile_refusals(harness: Harness, report: Report) -> None:
    base = {"schema": "agentctl-profiles/v1", "profiles": {
        "worker": {"harness": "codex", "mode": "interactive", "argv": [], "env": {}}
    }}
    for label in ("nonignored", "permissions", "symlink", "unknown-field"):
        case = harness.case(f"primary-profile-refusal-{label}")
        for root in (case.python_root, case.rust_root):
            init_case_repository(root)
            if label != "nonignored":
                (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            directory = root / ".agentctl"
            directory.mkdir(mode=0o700)
            document = json.loads(json.dumps(base))
            if label == "unknown-field":
                document["profiles"]["worker"]["surprise"] = True
            target = directory / "profiles.json"
            if label == "symlink":
                real = directory / "real.json"
                real.write_text(json.dumps(document), encoding="utf-8")
                real.chmod(0o600)
                target.symlink_to(real.name)
            else:
                target.write_text(json.dumps(document), encoding="utf-8")
                target.chmod(0o644 if label == "permissions" else 0o600)
        outcomes = harness.invoke(case, ("profiles", "--cwd", "<ROOT>"))
        report.require(f"primary/profile/refusal/{label}",
                       all(outcome.returncode == 75 and "traceback" not in outcome.stderr.lower()
                           and "panicked" not in outcome.stderr.lower() for outcome in outcomes),
                       f"unsafe profile config was accepted or crashed: {outcomes!r}")

    overlap = harness.case("primary-profile-refusal-overlap")
    for root in (overlap.python_root, overlap.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        directory = root / ".agentctl"
        directory.mkdir(mode=0o700)
        config = directory / "profiles.json"
        config.write_text(json.dumps(base), encoding="utf-8")
        config.chmod(0o600)
    outcomes = harness.invoke(overlap, (
        "start", "worker", "--cwd", "<ROOT>", "--profile", "worker", "--model", "override",
        *FIXTURE_HERDR,
    ))
    report.require("primary/profile/refusal/precedence",
                   all(outcome.returncode == 2 for outcome in outcomes),
                   f"profile precedence was ambiguous: {outcomes!r}")
    direct_precedence = (
        ("model-short-attached", "codex", ("--model", "structured"), ("-mother-model",)),
        ("effort-config-attached", "codex", ("--reasoning-effort", "ultra"),
         ("--config=model_reasoning_effort=low",)),
        ("model-quoted-config-key", "codex", ("--model", "structured"),
         ('--config="model"="attacker"',)),
        ("effort-quoted-config-key", "codex", ("--reasoning-effort", "ultra"),
         ('--config="model_reasoning_effort"="low"',)),
        ("model-opaque-config", "codex", ("--model", "structured"),
         ("--config=sandbox_mode=read-only",)),
        ("codex-resume", "codex", ("--resume", "session-1"), ("resume",)),
        ("muse-resume", "muse", ("--resume", "session-1"), ("resume",)),
        ("claude-resume", "claude", ("--resume", "session-1"), ("--continue",)),
    )
    for label, kind, structured, raw in direct_precedence:
        case = harness.case(f"primary-direct-precedence-{label}")
        arguments = [
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--harness", kind, *structured,
        ]
        for item in raw:
            arguments.append(f"--harness-arg={item}")
        outcomes = harness.invoke(case, (*arguments, *_COMMON))
        report.require(f"primary/profile/refusal/direct-{label}",
                       all(outcome.returncode == 75 for outcome in outcomes),
                       f"direct launch precedence was ambiguous: {outcomes!r}")
        report.require(
            f"primary/profile/refusal/direct-{label}-atomic",
            all(not (root / "registry").exists()
                for root in (case.python_root, case.rust_root)),
            "rejected direct launch allocated registry state",
        )

    adopt_muse = harness.case("primary-adopt-muse-mismatched-harness")
    outcomes = harness.invoke(adopt_muse, (
        "adopt", "worker", "--pane", "w1:p1", "--workspace", "project",
        "--cwd", "<ROOT>", "--harness", "muse", *_COMMON,
    ))
    report.require(
        "primary/adopt/muse-mismatched-harness",
        all(outcome.returncode == 75 for outcome in outcomes)
        and all(not (root / "registry/worker").exists()
                for root in (adopt_muse.python_root, adopt_muse.rust_root)),
        f"a different harness was adopted as Muse: {outcomes!r}",
    )

    hostile_path = harness.case("primary-profile-hostile-path", {"hostile_path": True})
    for root in (hostile_path.python_root, hostile_path.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        directory = root / ".agentctl"
        directory.mkdir(mode=0o700)
        config = directory / "profiles.json"
        config.write_text(json.dumps(base), encoding="utf-8")
        config.chmod(0o600)
        hostile = root / "hostile-bin"
        hostile.mkdir(mode=0o700)
        fake_git = hostile / "git"
        fake_git.write_text(
            f"#!/bin/sh\ntouch {root / 'hostile-git-executed'}\nexit 0\n",
            encoding="utf-8",
        )
        fake_git.chmod(0o700)
    _pair(harness, report, hostile_path, "primary/profile/hostile-path", (
        "profiles", "--cwd", "<ROOT>",
    ))
    report.require("primary/profile/hostile-path-not-executed", all(
        not (root / "hostile-git-executed").exists()
        for root in (hostile_path.python_root, hostile_path.rust_root)
    ), "profile discovery executed Git from caller PATH")


def _workspace_retirement(harness: Harness, report: Report) -> None:
    """A record started before its project gained a mismatched workspace key stays manageable.

    The goal query names the fixture's goal transport. Without it both editions fall back to the
    default ``codex app-server proxy`` on PATH, which is the developer's real Codex CLI.
    """
    retirement = harness.case("primary-workspace-retirement")
    for root in (retirement.python_root, retirement.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        (root / ".agentctl").mkdir(mode=0o700)
    common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/.agentctl")
    _pair(harness, report, retirement, "primary/workspace/retirement/start", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common,
    ))
    for root in (retirement.python_root, retirement.rust_root):
        config = root / ".agentctl/profiles.json"
        config.write_text(json.dumps({
            "schema": "agentctl-profiles/v1",
            "profiles": {"broken": {"harness": "codex"}},
            "workspace": "required-label",
        }), encoding="utf-8")
        config.chmod(0o600)
    _pair(harness, report, retirement, "primary/workspace/retirement/status", (
        "status", "worker", *common,
    ))
    _pair(harness, report, retirement, "primary/workspace/retirement/attach", (
        "attach", "worker", *common,
    ))
    python, _ = _pair(harness, report, retirement, "primary/workspace/retirement/goal-query", (
        "goal", "worker", *_GOAL_COMMAND, *common,
    ))
    goal = _json(python)
    report.require(
        "primary/workspace/retirement/goal-query-native",
        isinstance(goal, dict) and goal.get("source") == "native"
        and goal.get("native_status") == "absent" and "native_error" not in goal,
        f"the goal query did not reach the fixture's native goal transport: {goal!r}",
    )
    _pair(harness, report, retirement, "primary/workspace/retirement/wait", (
        "wait", "worker", "--timeout", "0", *common,
    ))
    _pair(harness, report, retirement, "primary/workspace/retirement/stop", (
        "stop", "worker", *common,
    ))


def _workspace_policy(harness: Harness, report: Report) -> None:
    """Pin the paired CLI surface for the optional project workspace key."""
    base = {"schema": "agentctl-profiles/v1", "profiles": {
        "worker": {"harness": "codex", "mode": "interactive", "argv": [], "env": {}}
    }}
    documents: dict[str, object] = {
        "valid": {"schema": "agentctl-profiles/v1", "profiles": {}, "workspace": "project"},
        "null": {"schema": "agentctl-profiles/v1", "profiles": {}, "workspace": None},
        "invalid": {"schema": "agentctl-profiles/v1", "profiles": {}, "workspace": " bad "},
    }
    for label, document in documents.items():
        case = harness.case(f"primary-workspace-{label}")
        for root in (case.python_root, case.rust_root):
            init_case_repository(root)
            (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            directory = root / ".agentctl"
            directory.mkdir(mode=0o700)
            config = directory / "profiles.json"
            config.write_text(json.dumps(document), encoding="utf-8")
            config.chmod(0o600)
        _pair(
            harness, report, case, f"primary/workspace/{label}/profiles",
            ("profiles", "--cwd", "<ROOT>"), 75 if label == "invalid" else 0,
        )

    malformed = harness.case("primary-workspace-malformed-unrelated-profile")
    for root in (malformed.python_root, malformed.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        directory = root / ".agentctl"
        directory.mkdir(mode=0o700)
        config = directory / "profiles.json"
        config.write_text(json.dumps({
            "schema": "agentctl-profiles/v1", "workspace": "project",
            "profiles": {"broken": {"harness": "codex"}},
        }), encoding="utf-8")
        config.chmod(0o600)
    _pair(harness, report, malformed, "primary/workspace/malformed/list", (
        "list", "--herdr-bin", "<HERDR>", "--registry", "<ROOT>/.agentctl",
    ))

    duplicate_workspace = harness.case("primary-workspace-duplicate-key")
    for root in (duplicate_workspace.python_root, duplicate_workspace.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        directory = root / ".agentctl"
        directory.mkdir(mode=0o700)
        config = directory / "profiles.json"
        config.write_text(
            '{"schema":"agentctl-profiles/v1","workspace":"first",'
            '"workspace":"second","profiles":{"broken":{"harness":"codex"}}}',
            encoding="utf-8",
        )
        config.chmod(0o600)
    _pair(
        harness, report, duplicate_workspace,
        "primary/workspace/duplicate-workspace-key",
        ("profiles", "--cwd", "<ROOT>"),
        75,
    )

    mismatch = harness.case("primary-workspace-explicit-mismatch")
    for root in (mismatch.python_root, mismatch.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        directory = root / ".agentctl"
        directory.mkdir(mode=0o700)
        config = directory / "profiles.json"
        config.write_text(json.dumps({
            "schema": "agentctl-profiles/v1",
            "profiles": {"broken": {"harness": "codex"}},
            "workspace": "required-label",
        }), encoding="utf-8")
        config.chmod(0o600)
    outcomes = harness.invoke(mismatch, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--herdr-bin", "<HERDR>", "--registry", "<ROOT>/.agentctl",
    ))
    report.require(
        "primary/workspace/explicit-mismatch",
        all(outcome.returncode == 75 and "project configuration requires" in outcome.stderr
            for outcome in outcomes)
        and all(not (root / ".agentctl/worker").exists()
                for root in (mismatch.python_root, mismatch.rust_root)),
        f"workspace mismatch diverged or allocated a record: {outcomes!r}",
    )

    _workspace_retirement(harness, report)

    move = harness.case("primary-workspace-move")
    for root in (move.python_root, move.rust_root):
        init_case_repository(root)
        (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
        (root / ".agentctl").mkdir(mode=0o700)
    move_common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/.agentctl")
    _pair(harness, report, move, "primary/workspace/move/start", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        *move_common,
    ))
    for root in (move.python_root, move.rust_root):
        config = root / ".agentctl/profiles.json"
        config.write_text(json.dumps({
            "schema": "agentctl-profiles/v1",
            "workspace": "project-agents",
            "profiles": {},
        }), encoding="utf-8")
        config.chmod(0o600)
    _pair(harness, report, move, "primary/workspace/move/command", (
        "move", "worker", *move_common,
    ))
    _pair(harness, report, move, "primary/workspace/move/status", (
        "status", "worker", *move_common,
    ))
    report.require(
        "primary/workspace/move/identity",
        all(_state(root).get("pane_id") == "w-project:p1"
            and _state(root).get("workspace_id") == "w-project"
            for root in (move.python_root, move.rust_root)),
        "move did not commit the destination Herdr identity",
    )

    strict_documents = {
        "duplicate-json": (
            '{"schema":"agentctl-profiles/v1","profiles":{'
            '"worker":{"harness":"codex","mode":"interactive"},'
            '"worker":{"harness":"muse","mode":"interactive"}}}'
        ),
        "reserved-headless": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "muse", "mode": "headless",
                       "argv": ["--session-id=attacker"], "env": {}}
        }}),
        "headless-option-terminator": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "muse", "mode": "headless", "argv": ["--"], "env": {}}
        }}),
        "headless-secret-stdin": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "muse", "mode": "headless",
                       "argv": ["--api-key-stdin"], "env": {}}
        }}),
        "unsupported-combination": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "claude", "mode": "headless", "argv": [], "env": {}}
        }}),
        "unsupported-interactive-agy": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "agy", "mode": "interactive", "argv": [], "env": {}}
        }}),
        "unsupported-headless-codex-argv": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "headless",
                       "argv": ["--search"], "env": {}}
        }}),
        "unsupported-headless-codex-effort": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "headless",
                       "reasoning_effort": "high", "argv": [], "env": {}}
        }}),
        "unsupported-headless-agy-effort": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "agy", "mode": "headless",
                       "reasoning_effort": "high", "argv": [], "env": {}}
        }}),
        "unsupported-headless-environment": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "muse", "mode": "headless", "argv": [],
                       "env": {"ROUTING_SELECTOR": "route"}}
        }}),
        "codex-model-short-attached": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "model": "structured",
                       "argv": ["-mother-model"], "env": {}}
        }}),
        "codex-model-config-attached": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "model": "structured",
                       "argv": ['--config= model = "other"'], "env": {}}
        }}),
        "codex-effort-config-short": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "reasoning_effort": "ultra",
                       "argv": ['-c=model_reasoning_effort = "low"'], "env": {}}
        }}),
        "codex-effort-config-split": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "reasoning_effort": "ultra",
                       "argv": ["--config", ' model_reasoning_effort = "low"'], "env": {}}
        }}),
        "codex-model-config-quoted": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "model": "structured",
                       "argv": ["--config", '\"model\"=\"other\"'], "env": {}}
        }}),
        "codex-effort-config-quoted": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "reasoning_effort": "ultra",
                       "argv": ['-c"model_reasoning_effort"="low"'], "env": {}}
        }}),
        "codex-opaque-config-profile": json.dumps({"schema": "agentctl-profiles/v1", "profiles": {
            "worker": {"harness": "codex", "mode": "interactive", "model": "structured",
                       "argv": ["-powner-defaults"], "env": {}}
        }}),
        "oversized": json.dumps(base) + (" " * (256 * 1024)),
    }
    for label, content in strict_documents.items():
        case = harness.case(f"primary-profile-strict-{label}")
        for root in (case.python_root, case.rust_root):
            init_case_repository(root)
            (root / ".gitignore").write_text(".agentctl/\n", encoding="utf-8")
            directory = root / ".agentctl"
            directory.mkdir(mode=0o700)
            config = directory / "profiles.json"
            config.write_text(content, encoding="utf-8")
            config.chmod(0o600)
        outcomes = harness.invoke(case, ("profiles", "--cwd", "<ROOT>"))
        report.require(f"primary/profile/strict/{label}",
                       all(outcome.returncode == 75 for outcome in outcomes),
                       f"ambiguous or unbounded profile was accepted: {outcomes!r}")

    missing = harness.case(
        "primary-custom-missing",
        {"missing_custom_executable": True, "empty_shell": True},
    )
    outcomes = harness.invoke(missing, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--harness", "muse", *_COMMON,
    ))
    report.require("primary/custom/missing-executable",
                   all(outcome.returncode == 75
                   and "cannot inspect muse executable" in outcome.stderr
                       for outcome in outcomes),
                   f"missing custom executable was not refused: {outcomes!r}")
    stopped = harness.invoke(missing, ("stop", "worker", *_COMMON))
    report.require("primary/custom/missing-executable-retained",
                   all(outcome.returncode == 69 for outcome in stopped)
                   and all((root / "registry/worker/agent.json").is_file()
                           and not _state(root).get("closed")
                           for root in (missing.python_root, missing.rust_root)),
                   f"unobserved missing-executable launch was incorrectly retired: {stopped!r}")

    wrong_process = harness.case(
        "primary-custom-wrong-process", {"wrong_custom_process": True}
    )
    outcomes = harness.invoke(wrong_process, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--harness", "muse", "--startup-timeout", "0.05", *_COMMON,
    ))
    report.require("primary/custom/exact-process",
                   all(outcome.returncode == 75 and "foreground process" in outcome.stderr
                       for outcome in outcomes),
                   f"lookalike custom process was accepted: {outcomes!r}")
    stopped = harness.invoke(wrong_process, ("stop", "worker", *_COMMON))
    report.require("primary/custom/exact-process-retained",
                   all(outcome.returncode == 69 for outcome in stopped)
                   and all((root / "registry/worker/agent.json").is_file()
                           and not _state(root).get("closed")
                           for root in (wrong_process.python_root, wrong_process.rust_root)),
                   f"unobserved wrong-process launch was incorrectly retired: {stopped!r}")
    _retire_fixture_processes(wrong_process)

    wrong_identity = harness.case(
        "primary-custom-wrong-kernel-identity", {"wrong_custom_process_identity": True}
    )
    outcomes = harness.invoke(wrong_identity, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--harness", "muse", "--startup-timeout", "0.05", *_COMMON,
    ))
    report.require("primary/custom/kernel-process-identity",
                   all(outcome.returncode == 75 and "foreground process" in outcome.stderr
                       for outcome in outcomes),
                   f"self-reported executable bypassed kernel identity: {outcomes!r}")
    stopped = harness.invoke(wrong_identity, ("stop", "worker", *_COMMON))
    report.require("primary/custom/kernel-process-retained",
                   all(outcome.returncode == 69 for outcome in stopped)
                   and all((root / "registry/worker/agent.json").is_file()
                           and not _state(root).get("closed")
                           for root in (wrong_identity.python_root, wrong_identity.rust_root)),
                   f"unobserved wrong-kernel launch was incorrectly retired: {stopped!r}")
    _retire_fixture_processes(wrong_identity)

    trust = harness.case("primary-muse-trust", {"screen": "Do you trust this workspace? yes / no"})
    outcomes = harness.invoke(trust, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--harness", "muse", *_COMMON,
    ))
    report.require("primary/muse/trust-refusal",
                   all(outcome.returncode == 75 and "trust prompt" in outcome.stderr
                       and _state(root).get("submitted") == []
                       for outcome, root in zip(outcomes, (trust.python_root, trust.rust_root), strict=True)),
                   f"Muse trust prompt was accepted or mutated: {outcomes!r}")
    stopped = harness.invoke(trust, ("stop", "worker", *_COMMON))
    report.require("primary/muse/failed-start-cleanup",
                   all(outcome.returncode == 0 for outcome in stopped)
                   and all(_state(root).get("closed_panes") == ["w1:p1"]
                           and not (root / "registry/worker").exists()
                           and len(list((root / "registry/archive").iterdir())) == 1
                           for root in (trust.python_root, trust.rust_root)),
                   f"retained failed Muse launch was not safely stoppable: {stopped!r}")

    exited = harness.case("primary-muse-exit-after-send-text")
    if _start(
        harness, report, exited, "primary/muse/exit-after-send-text/start",
        "--harness", "muse",
    ):
        _change(exited, {"custom_exit_after_send_text": True})
        outcomes = harness.invoke(exited, (
            "send", "worker", "must never reach a replacement shell",
            "--message-id", "exit-race", *_COMMON,
        ))
        report.require(
            "primary/muse/exit-after-send-text/no-enter",
            all(outcome.returncode == 76 for outcome in outcomes)
            and all(
                _state(root).get("custom_submitted") is False
                and _state(root).get("submitted") == []
                and (root / "registry/worker/queue/failed/exit-race.json").is_file()
                for root in (exited.python_root, exited.rust_root)
            ),
            f"Muse exit between text and Enter submitted to a replacement: {outcomes!r}",
        )
        _retire_fixture_processes(exited)


def _herdr_093_compatibility(harness: Harness, report: Report) -> None:
    """Exercise the 0.9.3 startup envelope and missing optional process argv."""
    unknown: dict[str, object] = {
        "status": "unknown", "startup_timeout": True, "empty_shell": True,
        "codex_footer": "  model default · /work/project",
        "foreground_argv_mode": "missing",
    }
    case = harness.case("primary-herdr-093-codex", unknown)
    started, _ = _pair(harness, report, case, "primary/herdr-093/codex/start", (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
        "--startup-timeout", "0.1", "--brief", "compatibility brief", *_COMMON,
    ))
    status = _json(started)
    report.require("primary/herdr-093/codex/raw-status-and-once",
                   isinstance(status, dict) and status.get("lifecycle") == "running"
                   and status.get("agent_status") == "unknown"
                   and all(_submission_count(root, "compatibility brief") == 1
                           and sum(call[:2] == ["agent", "start"]
                                   for call in _fixture_calls(root)) == 1
                           for root in (case.python_root, case.rust_root)),
                   f"unknown Codex was relabeled, relaunched, or lost its brief: {status!r}")
    if started.returncode == 0:
        _change(case, {"composer_busy": True})
        _pair(harness, report, case, "primary/herdr-093/codex/busy-pending", (
            "send", "worker", "compatibility follow-up", "--message-id", "compat-next",
            "--ready-timeout", "0", *_COMMON,
        ), 75)
        _pair(harness, report, case, "primary/herdr-093/codex/busy-drain", (
            "drain", "worker", "--ready-timeout", "0", *_COMMON,
        ), 75)
        report.require("primary/herdr-093/codex/busy-no-input", all(
            _submission_count(root, "compatibility follow-up") == 0
            and (root / "registry/worker/queue/inbox/compat-next.json").is_file()
            for root in (case.python_root, case.rust_root)),
            "an unknown busy composer received a queued follow-up")
        _change(case, {"composer_busy": False})
        _pair(harness, report, case, "primary/herdr-093/codex/idle-drain", (
            "drain", "worker", "--ready-timeout", "0", *_COMMON,
        ))
        report.require("primary/herdr-093/codex/drain-once", all(
            _submission_count(root, "compatibility follow-up") == 1
            and (root / "registry/worker/queue/processed/compat-next.json").is_file()
            for root in (case.python_root, case.rust_root)),
            "idle unknown Codex did not acknowledge the queued follow-up exactly once")
        _pair(harness, report, case, "primary/herdr-093/codex/stop", (
            "stop", "worker", *_COMMON,
        ))
    else:
        _retire_fixture_processes(case)

    refused: tuple[tuple[str, Mapping[str, object]], ...] = (
        ("busy", {"composer_busy": True}),
        ("hidden-interrupt", {"composer_busy": True, "codex_activity": "• Exploring (7s)"}),
        ("padded-hidden-interrupt", {"composer_busy": True,
                                    "codex_activity": "Working (7s)   "}),
        ("annotated-hidden-interrupt", {"composer_busy": True,
                                       "codex_activity": "Processing inputs (7s) · command (background)"}),
        ("remapped-interrupt", {"composer_busy": True,
                               "codex_activity": "• Exploring (1m 2s • Ctrl+c to interrupt)"}),
        ("queued-turn", {"composer_busy": True,
                         "codex_activity": "• Messages to be submitted at end of turn"}),
        ("grouped-menu", {"composer_busy": True,
                          "codex_activity": "All Results   Filesystem Only   Plugins"}),
        ("draft", {"composer_draft": "operator draft"}),
        ("unrecognized-footer", {"codex_footer": "unrecognized screen"}),
        ("terminal-swap", {"terminal_on_visible_read": "term-replacement"}),
        ("other-error", {"startup_error": {
            "id": "cli:agent:start", "error": {
                "code": "agent_not_ready", "message": "timed out waiting for agent startup",
            },
        }}),
        ("wrong-id", {"startup_error": {
            "id": "cli:request", "error": {
                "code": "timeout", "message": "timed out waiting for agent startup",
            },
        }}),
    )
    for label, overrides in refused:
        failed = harness.case(f"primary-herdr-093-codex-{label}", {**unknown, **overrides})
        _pair(harness, report, failed, f"primary/herdr-093/codex/refuse-{label}", (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--startup-timeout", "0.1", "--brief", "must remain unsent", *_COMMON,
        ), 75)
        report.require(f"primary/herdr-093/codex/refuse-{label}-atomic", all(
            _state(root).get("submitted") == []
            and sum(call[:2] == ["agent", "start"]
                    for call in _fixture_calls(root)) == 1
            and json.loads((root / "registry/worker/agent.json").read_text(
                encoding="utf-8")).get("lifecycle") == "launch_failed"
            for root in (failed.python_root, failed.rust_root)),
            f"{label} caused a startup retry, input, or a running record")
        _retire_fixture_processes(failed)

    for mode in ("missing", "null", "empty"):
        muse = harness.case(f"primary-herdr-093-muse-{mode}", {
            "foreground_argv_mode": mode,
        })
        if _start(harness, report, muse, f"primary/herdr-093/muse/{mode}/start",
                  "--harness", "muse", "--startup-timeout", "0.2"):
            report.require(f"primary/herdr-093/muse/{mode}/kernel-pin", all(
                _valid_foreign_shell_identity(json.loads(
                    (root / "registry/worker/agent.json").read_text(encoding="utf-8")
                ).get("custom_process_identity"))
                for root in (muse.python_root, muse.rust_root)),
                "missing argv discarded the custom harness kernel identity")
            _pair(harness, report, muse, f"primary/herdr-093/muse/{mode}/send", (
                "send", "worker", "optional argv delivery", "--message-id", "optional-argv",
                *_COMMON,
            ))
            report.require(f"primary/herdr-093/muse/{mode}/receipt", all(
                _submission_count(root, "optional argv delivery") == 1
                and _state(root).get("paste_wrapped") is True
                and (root / "registry/worker/queue/processed/optional-argv.json").is_file()
                for root in (muse.python_root, muse.rust_root)),
                "missing argv bypassed verified Muse delivery")
            _pair(harness, report, muse, f"primary/herdr-093/muse/{mode}/stop", (
                "stop", "worker", *_COMMON,
            ))
        else:
            _retire_fixture_processes(muse)

    malformed: tuple[tuple[str, Mapping[str, object]], ...] = (
        ("wrong-kernel", {"foreground_argv_mode": "missing",
                          "wrong_custom_process_identity": True}),
        ("argv-object", {"foreground_argv_override": {"0": "muse"}}),
        ("argv-nonstring", {"foreground_argv_override": ["muse", 42]}),
        ("argv-empty-first", {"foreground_argv_override": [""]}),
    )
    for label, state in malformed:
        failed = harness.case(f"primary-herdr-093-muse-{label}", state)
        outcomes = harness.invoke(failed, (
            "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1",
            "--harness", "muse", "--startup-timeout", "0.05", *_COMMON,
        ))
        report.require(f"primary/herdr-093/muse/refuse-{label}",
                       outcomes[0].returncode == outcomes[1].returncode
                       and outcomes[0].returncode in (69, 75)
                       and all(_state(root).get("submitted") == []
                               and json.loads((root / "registry/worker/agent.json").read_text(
                                   encoding="utf-8")).get("lifecycle") == "launch_failed"
                               for root in (failed.python_root, failed.rust_root)),
                       f"{label} became a verified Muse launch: {outcomes!r}")
        _retire_fixture_processes(failed)


def _muse_adoption(harness: Harness, report: Report) -> None:
    """A foreign Muse keeps its runtime and shares verified composer delivery."""
    failures: tuple[tuple[str, Mapping[str, object]], ...] = (
        ("missing-process", {"custom_identity_hidden": True}),
        ("wrong-process-group", {"wrong_custom_process_group": True}),
        ("missing-terminal", {"terminal_id": ""}),
    )
    for label, overrides in failures:
        refused = harness.case(f"primary-muse-adoption-{label}", {
            "harness": "muse", "custom_reported": True, "report_custom_session": True,
            "foreground_argv_mode": "missing", **overrides,
        })
        for root in (refused.python_root, refused.rust_root):
            subprocess.run([
                str(root / "fake-herdr"), "pane", "run", "w1:p1",
                shlex.join([str(root / "fake-muse-runtime")]),
            ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        _pair(harness, report, refused, f"primary/adopt/muse/refuse-{label}", (
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "muse", "--session", "session-1", *_COMMON,
        ), 75)
        report.require(f"primary/adopt/muse/refuse-{label}-atomic", all(
            not (root / "registry/foreign").exists()
            and not _state(root).get("closed")
            and not any(call[:2] in (["agent", "prompt"], ["pane", "send-text"],
                                     ["pane", "send-keys"], ["pane", "report-agent"],
                                     ["agent", "rename"], ["tab", "rename"])
                        for call in _fixture_calls(root))
            for root in (refused.python_root, refused.rust_root)),
            "unanchored Muse adoption saved a record or changed the foreign runtime")
        _retire_fixture_processes(refused)
    for label, with_session in (("native-session", True), ("sessionless", False)):
        case = harness.case(f"primary-muse-adoption-{label}", {
            "harness": "muse", "custom_reported": True,
            "report_custom_session": with_session, "foreground_argv_mode": "missing",
        })
        for root in (case.python_root, case.rust_root):
            subprocess.run([
                str(root / "fake-herdr"), "pane", "run", "w1:p1",
                shlex.join([str(root / "fake-muse-runtime")]),
            ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        session = ("--session", "session-1") if with_session else ()
        adopted, _ = _pair(harness, report, case, f"primary/adopt/muse/{label}/register", (
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "muse", *session, *_COMMON,
        ))
        record = _json(adopted)
        report.require(f"primary/adopt/muse/{label}/anchors", isinstance(record, dict)
                       and record.get("adapter") == "herdr-foreign"
                       and isinstance(record.get("harness_identity"), dict)
                       and _valid_foreign_shell_identity(record.get("foreign_shell_identity"))
                       and all(json.loads((root / "registry/foreign/agent.json").read_text(
                           encoding="utf-8")).get("custom_process_identity") is None
                           for root in (case.python_root, case.rust_root)),
                       f"adoption changed ownership or omitted mandatory Muse pins: {record!r}")
        if adopted.returncode != 0:
            _retire_fixture_processes(case)
            continue
        _pair(harness, report, case, f"primary/adopt/muse/{label}/send", (
            "send", "foreign", "foreign Muse literal mentions Auto-review\nsecond line",
            "--message-id", "foreign-muse",
            *_COMMON,
        ))
        report.require(f"primary/adopt/muse/{label}/guarded-receipt", all(
            _submission_count(root, "foreign Muse literal mentions Auto-review\nsecond line") == 1
            and _state(root).get("paste_wrapped") is True
            and (root / "registry/foreign/queue/processed/foreign-muse.json").is_file()
            and not any(call[:2] in (["agent", "prompt"], ["pane", "report-agent"],
                                     ["agent", "rename"], ["tab", "rename"])
                        for call in _fixture_calls(root))
            for root in (case.python_root, case.rust_root)),
            "foreign Muse bypassed staged input or changed runtime ownership")
        _change(case, {"custom_permission": "YOLO", "custom_submitted": False,
                       "custom_draft": ""})
        _pair(harness, report, case, f"primary/adopt/muse/{label}/yolo-refusal", (
            "send", "foreign", "must not bypass Auto-review", "--message-id", "foreign-yolo",
            "--ready-timeout", "0", *_COMMON,
        ), 76)
        report.require(f"primary/adopt/muse/{label}/yolo-no-input", all(
            _submission_count(root, "must not bypass Auto-review") == 0
            and (root / "registry/foreign/queue/failed/foreign-yolo.json").is_file()
            for root in (case.python_root, case.rust_root)),
            "foreign Muse delivery ignored Auto-review policy")
        _change(case, {"custom_permission": "Auto-review",
                       "fixture_shell_pid": harness.replacement_shell.pid})
        outcomes = harness.invoke(case, ("send", "foreign", "must not cross shell generation",
                                         "--message-id", "foreign-replacement", *_COMMON))
        report.require(f"primary/adopt/muse/{label}/shell-replacement-refusal",
                       all(outcome.returncode == 75 for outcome in outcomes)
                       and all(_submission_count(root, "must not cross shell generation") == 0
                               for root in (case.python_root, case.rust_root)),
                       f"foreign Muse input crossed a shell generation: {outcomes!r}")
        _change(case, {"fixture_shell_pid": harness.fixture_shell.pid})
        stopped, _ = _pair(harness, report, case, f"primary/adopt/muse/{label}/unregister", (
            "stop", "foreign", *_COMMON,
        ))
        result = _json(stopped)
        report.require(f"primary/adopt/muse/{label}/runtime-preserved",
                       isinstance(result, dict) and result.get("runtime_preserved") is True
                       and all(not _state(root).get("closed")
                               and not (root / "registry/foreign").exists()
                               and len(list((root / "registry/archive").glob("*/agent.json"))) == 1
                               for root in (case.python_root, case.rust_root)),
                       f"foreign Muse unregister closed or discarded its runtime: {result!r}")
        _retire_fixture_processes(case)


def _muse14_status(harness: Harness, report: Report) -> None:
    """Muse 1.4 frames read as paused, idle, staged or working alike; only idle takes input."""
    case = harness.case("primary-muse14-status", {
        "harness": "muse", "custom_reported": True, "report_custom_session": True,
        "foreground_argv_mode": "missing",
    })
    for root in (case.python_root, case.rust_root):
        subprocess.run([
            str(root / "fake-herdr"), "pane", "run", "w1:p1",
            shlex.join([str(root / "fake-muse-runtime")]),
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    adopted, _ = _pair(harness, report, case, "primary/muse14/adopt", (
        "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
        "--cwd", "<ROOT>", "--harness", "muse", "--session", "session-1", *_COMMON,
    ))
    if adopted.returncode != 0:
        _retire_fixture_processes(case)
        return
    divider, footer = "─" * 40 + "\n", "kiki · xhigh · /work · YOLO\n"
    idle = f"old transcript\n{divider}❯\n{divider}{footer}"
    staged, paused = idle.replace("❯\n", "❯ owner draft\n"), idle.replace(footer, "Goal (paused)\n" + footer)
    headed = "Muse Code 1.4.0\n" + paused.replace("YOLO", "Auto-review")
    headed_staged = "Muse Code 1.4.0\n" + staged.replace("YOLO", "Auto-review")
    frames = (("idle", "done", idle, "idle"), ("staged", "done", staged, "staged"),
              ("paused", "done", paused, "paused"), ("echo", "done", "Goal (paused)\n" + idle, "idle"),
              ("busy-headerless", "working", paused, "working"), ("busy-headed", "working", headed, "idle"),
              ("headed-paused", "done", headed, "paused"), ("headed-staged", "idle", headed_staged, "staged"))
    for label, status, screen, expected in frames:
        _change(case, {"status": status, "screen": screen})
        outcomes = _pair(harness, report, case, f"primary/muse14/{label}/status",
                         ("status", "foreign", *_COMMON))
        report.require(f"primary/muse14/{label}/agent-status", all(
            isinstance(value := _json(outcome), dict) and value.get("agent_status") == expected
            for outcome in outcomes), f"expected {expected}: {outcomes!r}")
    for label, screen in (("paused", headed), ("staged", headed_staged)):
        _change(case, {"status": "done", "screen": screen})
        _pair(harness, report, case, f"primary/muse14/{label}/held", (
            "send", "foreign", f"held while {label}", "--message-id", f"muse14-{label}",
            "--ready-timeout", "0", *_COMMON,
        ), 75)
    report.require("primary/muse14/held-without-input", all(
        not any(call[:2] in (["agent", "prompt"], ["pane", "send-text"], ["pane", "send-keys"])
                for call in _fixture_calls(root))
        and len(list((root / "registry/foreign/queue/inbox").glob("*.json"))) == 2
        for root in (case.python_root, case.rust_root)), "a paused or staged Muse editor took input")
    _change(case, {"status": "idle", "screen": ""})
    _pair(harness, report, case, "primary/muse14/resumed-drain", ("drain", "foreign", *_COMMON))
    report.require("primary/muse14/delivered-once-in-order", all(
        _state(root).get("submitted") == ["held while paused", "held while staged"]
        for root in (case.python_root, case.rust_root)), "held Muse input was lost, repeated or reordered")
    _retire_fixture_processes(case)


def _handoff_and_pending(harness: Harness, report: Report) -> None:
    case = harness.case("primary-handoff")
    if not _start(harness, report, case, "primary/handoff/start"):
        return
    _change(case, {"status": "working"})
    _pair(harness, report, case, "primary/handoff/busy-pending",
          ("send", "worker", "queued before handoff", "--message-id", "waiting", "--ready-timeout", "0", *_COMMON), 75)
    _pair(harness, report, case, "primary/handoff/pause", ("pause", "worker", *_COMMON))
    for command in (("send", "worker", "refuse while paused"), ("drain", "worker"), ("goal", "worker", "refuse new goal")):
        _pair(harness, report, case, f"primary/handoff/paused-{command[0]}", (*command, *_COMMON), 75)
    _pair(harness, report, case, "primary/handoff/attach", ("attach", "worker", *_COMMON))
    _pair(harness, report, case, "primary/handoff/read", ("read", "worker", *_COMMON))
    report.require("primary/handoff/preserved-input", all(_state(root).get("submitted") == []
                   and _state(root).get("focused") == "w1:p1" for root in (case.python_root, case.rust_root)),
                   "handoff injected input or failed to focus the owned pane")
    _change(case, {"status": "idle"})
    _pair(harness, report, case, "primary/handoff/resume", ("resume", "worker", *_COMMON))
    _pair(harness, report, case, "primary/handoff/drain", ("drain", "worker", *_COMMON))
    report.require("primary/handoff/pending-once", all(_state(root).get("submitted") == ["queued before handoff"]
                   for root in (case.python_root, case.rust_root)) and
                   _queue_snapshot(case.python_root, "registry/worker/queue") == _queue_snapshot(case.rust_root, "registry/worker/queue"),
                   "resume lost, duplicated, or changed the pending request")
    _change(case, {"run_mode": "fail"})
    _pair(harness, report, case, "primary/handoff/ambiguous",
          ("send", "worker", "uncertain request", "--message-id", "uncertain", *_COMMON), 76)
    _change(case, {"run_mode": "normal"})
    _pair(harness, report, case, "primary/handoff/no-replay", ("drain", "worker", *_COMMON))
    report.require("primary/handoff/uncertain-retained", all(
        _state(root).get("submitted") == ["queued before handoff", "uncertain request"]
        and (root / "registry/worker/queue/failed/uncertain.json").exists()
        for root in (case.python_root, case.rust_root)), "uncertain work was replayed or discarded")


def _registry_and_interop(harness: Harness, report: Report) -> None:
    defaults = harness.case("primary-default-registry")
    _pair(harness, report, defaults, "primary/default-registry/start", ("start", "worker", "--herdr-bin", "<HERDR>"))
    report.require("primary/default-registry/location", all((root / ".agentctl/worker/agent.json").is_file()
                   for root in (defaults.python_root, defaults.rust_root)), "default registry was not .agentctl")
    older = harness.case("primary-record-defaults")
    if _start(harness, report, older, "primary/old-record/start"):
        for root in (older.python_root, older.rust_root):
            path = root / "registry/worker/agent.json"
            record = json.loads(path.read_text(encoding="utf-8"))
            for key in ("adapter", "mode", "backend", "paused", "runtime_home"):
                record.pop(key, None)
            path.write_text(json.dumps(record), encoding="utf-8")
        python, _ = _pair(harness, report, older, "primary/old-record/defaults", ("status", "worker", *_COMMON))
        record = _json(python)
        report.require("primary/old-record/compatible", isinstance(record, dict) and all(record.get(key) == value
                       for key, value in {"adapter": "herdr", "mode": "interactive", "backend": "herdr",
                                          "paused": False, "runtime_home": None}.items()),
                       f"older records lost their interactive defaults: {record!r}")
    for option in ("--registry", "--state"):
        for before in (True, False):
            label = f"primary/global/{option}/{'before' if before else 'after'}"
            case = harness.case(label)
            shared = (option, "<ROOT>/chosen", "--herdr-bin", "<HERDR>")
            def command(*args: str) -> tuple[str, ...]:
                return (*shared, *args) if before else (*args, *shared)
            _pair(harness, report, case, label + "/start", command("start", "worker"))
            _pair(harness, report, case, label + "/status", command("status", "worker"))
            report.require(label + "/default-record", all(
                (root / "chosen/worker/agent.json").is_file() and not (root / ".agentctl").exists()
                and _state(root).get("launch_arguments") == ["--no-alt-screen"]
                for root in (case.python_root, case.rust_root)), "registry alias/placement or native defaults changed")
    for label, producer, consumer in (("python-rust", harness.python, harness.rust), ("rust-python", harness.rust, harness.python)):
        case = harness.case(f"primary/interop/{label}")
        root = case.python_root
        started = harness._invoke_one(producer, root, ("start", "worker", *_COMMON))
        path = root / "registry/worker/agent.json"
        if path.exists():
            record = json.loads(path.read_text(encoding="utf-8"))
            record["extension_metadata"] = {"purpose": "review", "labels": ["persistent"]}
            path.write_text(json.dumps(record), encoding="utf-8")
        paused = harness._invoke_one(consumer, root, ("pause", "worker", *_COMMON))
        refused = harness._invoke_one(producer, root, ("send", "worker", "human owns input", *_COMMON))
        resumed = harness._invoke_one(consumer, root, ("resume", "worker", *_COMMON))
        sent = harness._invoke_one(consumer, root, ("send", "worker", "shared primary registry", *_COMMON))
        stopped = harness._invoke_one(producer, root, ("stop", "worker", *_COMMON))
        outcomes = (started, paused, resumed, sent, stopped)
        report.require(f"primary/interop/{label}", all(outcome.returncode == 0 for outcome in outcomes)
                       and refused.returncode == 75 and _state(root).get("submitted") == ["shared primary registry"],
                       f"canonical registry/handoff incompatible: {outcomes!r}; refused={refused!r}")
        archives = list((root / "registry/archive").glob("*/agent.json"))
        report.require(f"primary/interop/{label}/extension-metadata", len(archives) == 1
                       and json.loads(archives[0].read_text(encoding="utf-8")).get("extension_metadata")
                       == {"purpose": "review", "labels": ["persistent"]},
                       "another edition discarded unknown record metadata during handoff/retirement")


def _legacy_adopted_registry_and_queue(harness: Harness, report: Report) -> None:
    """Read and drain one literal pre-profile adopted record and queue artifact."""

    case = harness.case("primary-legacy-adopted-registry")
    # The field set is derived from AgentRecord in pre-profile revision
    # 475c2498c2c54767f70e3056cf6fda668738205a. These are constructed test
    # artifacts rather than captured command output; each root has distinct
    # expected_cwd bytes, which are frozen before either edition reads them.
    for root in (case.python_root, case.rust_root):
        agent_root = root / "registry/foreign"
        queue = agent_root / "queue"
        for directory in (
            root / "registry", agent_root, queue,
            queue / "inbox", queue / "inflight", queue / "processed", queue / "failed",
        ):
            directory.mkdir(mode=0o700, exist_ok=True)
            directory.chmod(0o700)
        record = {
            "name": "foreign",
            "token": "legacy-token",
            "harness": "codex",
            "cwd": str(root),
            "created_at": 1.0,
            "schema": 1,
            "lifecycle": "running",
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "pane_id": "w1:p1",
            "session_agent": "codex",
            "session_value": "session-1",
            "model": None,
            "resume": None,
            "arguments": [],
            "error": None,
            "goal": None,
            "goal_delivery": None,
            "goal_session_id": None,
            "goal_command": None,
            "goal_messages": {},
            "goal_message_id": None,
            "adapter": "herdr-foreign",
            "mode": "interactive",
            "backend": "herdr",
            "paused": False,
            "runtime_home": None,
        }
        binding = {
            "kind": "session",
            "agent": "codex",
            "value": "session-1",
            "expected_agent": "codex",
            "expected_workspace": None,
            "expected_cwd": str(root),
        }
        artifacts = {
            agent_root / "agent.json": json.dumps(record, sort_keys=True) + "\n",
            queue / "target.json": json.dumps(binding, sort_keys=True) + "\n",
            queue / "inbox/000000000007.json": (
                '{"seq":7,"text":"legacy fifo","tui_delivery_attempts":0}\n'
            ),
        }
        for path, content in artifacts.items():
            path.write_text(content, encoding="utf-8")
            path.chmod(0o600)

    immutable_paths = ("agent.json", "queue/target.json")
    immutable_before = [
        {
            relative: (root / "registry/foreign" / relative).read_bytes()
            for relative in immutable_paths
        }
        for root in (case.python_root, case.rust_root)
    ]
    queue_before_status = [
        _queue_snapshot(root, "registry/foreign/queue")
        for root in (case.python_root, case.rust_root)
    ]

    python, _ = _pair(
        harness, report, case, "primary/legacy-adopted/status",
        ("status", "foreign", *_COMMON),
    )
    status = _json(python)
    report.require(
        "primary/legacy-adopted/status-shape",
        isinstance(status, dict)
        and status.get("adapter") == "herdr-foreign"
        and status.get("agent_status") == "idle"
        and status.get("pending") == ["000000000007"],
        f"pre-profile adopted record or pending queue was not readable: {status!r}",
    )
    immutable_after_status = [
        {
            relative: (root / "registry/foreign" / relative).read_bytes()
            for relative in immutable_paths
        }
        for root in (case.python_root, case.rust_root)
    ]
    queue_after_status = [
        _queue_snapshot(root, "registry/foreign/queue")
        for root in (case.python_root, case.rust_root)
    ]
    report.require(
        "primary/legacy-adopted/status-nonmutation",
        immutable_after_status == immutable_before
        and queue_after_status == queue_before_status,
        "status rewrote the pre-profile record, binding, or queue",
    )
    _pair(
        harness, report, case, "primary/legacy-adopted/drain",
        ("drain", "foreign", *_COMMON),
    )
    snapshots = [
        _queue_snapshot(root, "registry/foreign/queue")
        for root in (case.python_root, case.rust_root)
    ]
    immutable_after_drain = [
        {
            relative: (root / "registry/foreign" / relative).read_bytes()
            for relative in immutable_paths
        }
        for root in (case.python_root, case.rust_root)
    ]
    processed = snapshots[0].get("processed/000000000007.json")
    report.require(
        "primary/legacy-adopted/drained-once",
        all(_state(root).get("submitted") == ["legacy fifo"]
            and not list((root / "registry/foreign/queue/inbox").iterdir())
            and (root / "registry/foreign/queue/processed/000000000007.json").is_file()
            for root in (case.python_root, case.rust_root))
        and immutable_after_drain == immutable_before
        and snapshots[0] == snapshots[1]
        and set(snapshots[0]) == {"processed/000000000007.json"}
        and isinstance(processed, dict)
        and processed.get("seq") == 7
        and processed.get("text") == "legacy fifo"
        and processed.get("tui_delivery_attempts") == 0,
        f"pre-profile pending artifact diverged or was lost: {snapshots!r}",
    )
    _change(case, {"empty_shell": True})
    _pair(
        harness,
        report,
        case,
        "primary/legacy-adopted/dead-stop-refusal",
        ("stop", "foreign", *_COMMON),
        75,
    )
    report.require(
        "primary/legacy-adopted/dead-stop-preserved",
        all(
            (root / "registry/foreign/agent.json").is_file()
            and not _state(root).get("closed")
            and not _state(root).get("closed_panes")
            for root in (case.python_root, case.rust_root)
        ),
        "legacy adopted record without shell identity was retired or mutated",
    )

    _change(case, {"fail_read": True})
    capture_failures = []
    for command, root in (
        (harness.python, case.python_root), (harness.rust, case.rust_root),
    ):
        record_path = root / "registry/foreign/agent.json"
        digest = hashlib.sha256(record_path.read_bytes()).hexdigest()
        capture_failures.append(harness._invoke_one(command, root, (
            "stop", "foreign", "--recover-legacy-adoption",
            "--expected-token", "legacy-token",
            "--expected-record-sha256", digest,
            *_COMMON,
        )))
    report.require(
        "primary/legacy-adopted/capture-failure-is-busy",
        all(outcome.returncode == 75 for outcome in capture_failures)
        and all((root / "registry/foreign/agent.json").is_file()
                for root in (case.python_root, case.rust_root)),
        f"legacy recovery capture failures did not preserve delivery classification: {capture_failures!r}",
    )
    _change(case, {"fail_read": False})

    outcomes: list[Outcome] = []
    recovery_values: list[dict[str, object]] = []
    for command, root in (
        (harness.python, case.python_root), (harness.rust, case.rust_root),
    ):
        record_path = root / "registry/foreign/agent.json"
        digest = hashlib.sha256(record_path.read_bytes()).hexdigest()
        outcome = harness._invoke_one(command, root, (
            "stop", "foreign", "--recover-legacy-adoption",
            "--expected-token", "legacy-token",
            "--expected-record-sha256", digest,
            *_COMMON,
        ))
        outcomes.append(outcome)
        value = json.loads(outcome.stdout) if outcome.returncode == 0 else {}
        if isinstance(value, dict):
            report.require(
                f"primary/legacy-adopted/recovery-hash-{root.parent.name}",
                value.get("record_sha256") == digest,
                f"recovery did not report its exact record digest: {outcome!r}",
            )
            value["record_sha256"] = "<ROOT-SPECIFIC-HASH>"
        recovery_values.append(value)
    report.require(
        "primary/legacy-adopted/recovery-parity",
        all(outcome.returncode == 0 and not outcome.stderr for outcome in outcomes)
        and _normalize(recovery_values[0]) == _normalize(recovery_values[1]),
        f"legacy recovery diverged: {outcomes!r}",
    )
    report.require(
        "primary/legacy-adopted/recovery-preserves-bytes-and-runtime",
        all(
            not (root / "registry/foreign").exists()
            and not _state(root).get("closed")
            and not _state(root).get("closed_panes")
            and len(list((root / "registry/archive").glob("*/agent.json"))) == 1
            and list((root / "registry/archive").glob("*/agent.json"))[0].read_bytes()
                == immutable_after_drain[index]["agent.json"]
            and _queue_snapshot(
                list((root / "registry/archive").glob("*"))[0], "queue"
            ) == snapshots[index]
            for index, root in enumerate((case.python_root, case.rust_root))
        ),
        "explicit recovery changed legacy record/queue bytes or mutated its runtime",
    )


def _ownership(harness: Harness, report: Report) -> None:
    for label, change in (("unavailable", {"offline": True}), ("replacement", {"name": "different"}),
                          ("late-human-pane", {"extra_pane_on_read": True})):
        case = harness.case(f"primary/ownership/{label}")
        if not _start(harness, report, case, f"primary/ownership/{label}/start"):
            continue
        _change(case, change)
        if label == "late-human-pane":
            python, _ = _pair(harness, report, case, "primary/ownership/late-pane-stop", ("stop", "worker", *_COMMON))
            result = _json(python)
            report.require("primary/ownership/human-pane-preserved", isinstance(result, dict)
                           and result.get("pane_closed") is True and result.get("tab_closed") is False
                           and all(_state(root).get("closed_panes") == ["w1:p1"] and _state(root).get("extra_pane")
                                   for root in (case.python_root, case.rust_root)),
                           f"closing the owned pane also closed the human pane: {result!r}")
        else:
            _pair(harness, report, case, f"primary/ownership/{label}/status", ("status", "worker", *_COMMON))
            outcomes = harness.invoke(case, ("stop", "worker", *_COMMON))
            report.require(f"primary/ownership/{label}/stop-refusal", all(outcome.returncode == 69 for outcome in outcomes)
                           and all((root / "registry/worker/agent.json").exists() and not _state(root).get("closed")
                           for root in (case.python_root, case.rust_root)), f"uncertain identity was retired: {outcomes!r}")


def _adopted_generation(harness: Harness, report: Report, label: str) -> PairCase | None:
    """Produce foreign records through the CLI with real foreground generations."""
    case = harness.case(label, {"empty_shell": True, "sessionless": True})
    for root in (case.python_root, case.rust_root):
        subprocess.run([
            str(root / "fake-herdr"), "agent", "start", "external-runtime",
            "--kind", "codex", "--pane", "w1:p1", "--timeout", "1", "--",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    outcomes = _pair(harness, report, case, label + "/adopt", (
        "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
        "--cwd", "<ROOT>", "--harness", "codex", *_COMMON,
    ))
    if any(outcome.returncode != 0 for outcome in outcomes):
        _retire_fixture_processes(case)
        return None
    return case


def _record_document(path: Path) -> dict[str, object]:
    value: object = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise TypeError("fixture record must be an object")
    return {str(key): item for key, item in value.items()}


def _artifact_bytes(directory: Path) -> dict[str, bytes]:
    """Capture original artifacts, including arbitrary forensic file formats."""
    return {path.relative_to(directory).as_posix(): path.read_bytes()
            for path in directory.rglob("*") if path.is_file()}


def _dead_adopted_retirement(harness: Harness, report: Report) -> None:
    """Cross-consume raw adopted records without consulting the restarted server."""
    case = _adopted_generation(harness, report, "primary/dead-adopted")
    if case is None:
        return
    originals: list[dict[str, bytes]] = []
    tokens: list[str] = []
    digests: list[str] = []
    for root in (case.python_root, case.rust_root):
        directory = root / "registry/foreign"
        path = directory / "agent.json"
        document = _record_document(path)
        token = document["token"]
        if not isinstance(token, str):
            raise TypeError("fixture record token must be a string")
        tokens.append(token)
        document["future_metadata"] = {"preserve": "☃", "opaque": [1, None, True]}
        path.write_bytes((json.dumps(document, ensure_ascii=False, indent=1) + "\n \n").encode())
        artifacts = {
            "output.json": b'{"text":"saved output","captured_at":7}\n',
            "operator-note.bin": b"\x00\xffpreserve\r\n",
            "queue/.delivery.lock": b"",
            "queue/inbox/pending.json": b'{"id":"pending","text":"never replay","queued_at":1}\n',
            "queue/inflight/uncertain.json": b'{"id":"uncertain","text":"maybe submitted"}\n',
            "queue/failed/quarantined.json": b'{"id":"quarantined","text":"do not retry"}\n',
            "queue/failed/quarantined.json.error": b'{"outcome":"possibly_submitted"}\n',
            "queue/processed/done.json": b'{"id":"done","text":"already submitted"}\n',
        }
        for relative, content in artifacts.items():
            target = directory / relative
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            target.write_bytes(content)
            target.chmod(0o600)
        originals.append(_artifact_bytes(directory))
        digests.append(hashlib.sha256(path.read_bytes()).hexdigest())
    _retire_fixture_processes(case)
    _change(case, {
        "closed": False, "closed_panes": [], "empty_shell": True,
        "fixture_shell_pid": harness.replacement_shell.pid,
        "fixture_shell_executable": harness.fixture_shell_executable,
    })
    # Each consumer reads the other edition's producer record in its own fixture root.
    consumers = ((harness.rust, case.python_root), (harness.python, case.rust_root))
    refused = [harness._invoke_one(command, root, ("stop", "foreign", *_COMMON))
               for command, root in consumers]
    actions = [_stop_recovery_arguments(outcome) for outcome in refused]
    report.require("primary/dead-adopted/restart-advice", all(
        outcome.returncode == 75 and "recorded pane shell generation changed" in outcome.stderr
        and arguments is not None and "--retire-dead-adoption" in arguments
        and "--recover-legacy-adoption" not in arguments
        and _option_values(arguments, "--expected-token") == [token]
        and _option_values(arguments, "--expected-record-sha256") == [digest]
        for outcome, arguments, token, digest in zip(refused, actions, tokens, digests, strict=True)
    ), f"restart refused without exact dead-adoption advice: {refused!r}")
    _change(case, {"offline": True})
    offline_refused = [harness._invoke_one(command, root, ("stop", "foreign", *_COMMON))
                       for command, root in consumers]
    report.require("primary/dead-adopted/offline-advice", all(
        outcome.returncode == 69 and (arguments := _stop_recovery_arguments(outcome)) is not None
        and "--retire-dead-adoption" in arguments
        and _option_values(arguments, "--expected-token") == [token]
        and _option_values(arguments, "--expected-record-sha256") == [digest]
        for outcome, token, digest in zip(offline_refused, tokens, digests, strict=True)
    ), f"offline failure lost previously captured authority: {offline_refused!r}")
    calls = [_fixture_calls(root) for _command, root in consumers]
    for label, extra in (
        ("missing-assertions", ()),
        ("missing-digest", ("--expected-token", "stale-token")),
        ("both-selectors", ("--recover-legacy-adoption",)),
    ):
        outcomes = [harness._invoke_one(command, root, (
            "stop", "foreign", "--retire-dead-adoption", *extra, *_COMMON,
        )) for command, root in consumers]
        report.require("primary/dead-adopted/cli/" + label, all(
            outcome.returncode == 2
            and (arguments := _stop_recovery_arguments(outcome)) is not None
            and arguments[-1] == "doctor" for outcome in outcomes
        ), f"invalid retirement syntax did not refuse with advice: {outcomes!r}")
    for label, change in (("token", "token"), ("same-token-raw-bytes", "format")):
        for index, (_command, root) in enumerate(consumers):
            path = root / "registry/foreign/agent.json"
            content = originals[index]["agent.json"]
            if change == "token":
                document = _record_document(path)
                document["token"] = "replacement-token"
                path.write_text(json.dumps(document), encoding="utf-8")
            else:
                path.write_bytes(content + b"\n")
        outcomes = [
            harness._invoke_one(command, root, arguments[1:]) if arguments is not None
            else Outcome(1, "", "missing captured command")
            for (command, root), arguments in zip(consumers, actions, strict=True)
        ]
        report.require("primary/dead-adopted/stale-advice/" + label, all(
            outcome.returncode == 75
            and (arguments := _stop_recovery_arguments(outcome)) is not None
            and arguments[-1] == "doctor" for outcome in outcomes
        ) and all((root / "registry/foreign/agent.json").is_file()
                  and _fixture_calls(root) == calls[index]
                  for index, (_command, root) in enumerate(consumers)),
                       f"stale command acquired new record authority: {outcomes!r}")
        for index, (_command, root) in enumerate(consumers):
            (root / "registry/foreign/agent.json").write_bytes(originals[index]["agent.json"])
    retired = [
        harness._invoke_one(command, root, arguments[1:]) if arguments is not None
        else Outcome(1, "", "missing captured command")
        for (command, root), arguments in zip(consumers, actions, strict=True)
    ]
    values: list[object] = []
    for index, ((_command, root), outcome) in enumerate(zip(consumers, retired, strict=True)):
        value = _json(outcome)
        report.require(f"primary/dead-adopted/interop/{index}", outcome.returncode == 0
                       and not outcome.stderr and isinstance(value, dict)
                       and value.get("record_sha256") == digests[index]
                       and value.get("retired_dead_adoption") is True
                       and value.get("runtime_preserved") is True
                       and value.get("pane_closed") is False and value.get("tab_closed") is False,
                       f"dead adoption did not retire through the other edition: {outcome!r}")
        if isinstance(value, dict):
            value["record_sha256"] = "<RAW-RECORD-HASH>"
        values.append(value)
    report.require("primary/dead-adopted/result-parity", values[0] == values[1],
                   f"retirement result schema differs: {values!r}")
    report.require("primary/dead-adopted/whole-original-preserved", all(
        not (root / "registry/foreign").exists()
        and len(archived := list((root / "registry/archive").glob("*/agent.json"))) == 1
        and all((archived[0].parent / relative).read_bytes() == content
                for relative, content in originals[index].items())
        and _fixture_calls(root) == calls[index]
        and _state(root).get("submitted") == []
        and _state(root).get("closed_panes") == []
        for index, (_command, root) in enumerate(consumers)
    ) and harness.replacement_shell.poll() is None,
                   "retirement changed original bytes, replayed a prompt or touched the replacement shell")

    for label in ("live", "unknown-image", "missing-pin", "old-anchor", "nonrunning", "owned"):
        guarded = _adopted_generation(harness, report, "primary/dead-adopted/refuse-" + label)
        if guarded is None:
            continue
        assertions: list[tuple[str, str]] = []
        for root in (guarded.python_root, guarded.rust_root):
            path = root / "registry/foreign/agent.json"
            document = _record_document(path)
            if label == "unknown-image":
                identity = document.get("harness_identity")
                if not isinstance(identity, dict) or not isinstance(identity.get("executable_inode"), int):
                    raise TypeError("fixture harness identity is missing")
                identity["executable_inode"] += 1
            elif label == "missing-pin":
                document["harness_identity"] = None
            elif label == "old-anchor":
                document["anchor_rule"] = 1
            elif label == "nonrunning":
                document["lifecycle"] = "stopped"
            elif label == "owned":
                document["adapter"] = "herdr"
            path.write_text(json.dumps(document), encoding="utf-8")
            token = document["token"]
            if not isinstance(token, str):
                raise TypeError("fixture record token must be a string")
            assertions.append((token, hashlib.sha256(path.read_bytes()).hexdigest()))
        _change(guarded, {"offline": True})
        before = [_fixture_calls(root) for root in (guarded.python_root, guarded.rust_root)]
        outcomes = [harness._invoke_one(command, root, (
            "stop", "foreign", "--retire-dead-adoption", "--expected-token", token,
            "--expected-record-sha256", digest, *_COMMON,
        )) for command, root, (token, digest) in zip(
            (harness.python, harness.rust), (guarded.python_root, guarded.rust_root), assertions, strict=True,
        )]
        report.require("primary/dead-adopted/guard/" + label, all(
            outcome.returncode == 75
            and (arguments := _stop_recovery_arguments(outcome)) is not None
            and arguments[-1] == "doctor" for outcome in outcomes
        ) and all((root / "registry/foreign/agent.json").is_file()
                  and _fixture_calls(root) == before[index]
                  for index, root in enumerate((guarded.python_root, guarded.rust_root))),
                       f"{label} record was retired or queried Herdr: {outcomes!r}")
        _change(guarded, {"offline": False})
        _retire_fixture_processes(guarded)


def _managed_dead_recovery(harness: Harness, report: Report) -> None:
    case = harness.case("primary-managed-dead", {"empty_shell": False})
    if not _start(harness, report, case, "primary/managed-dead/start"):
        return
    _change(case, {"empty_shell": True, "sessionless": True})
    refused = harness.invoke(case, ("stop", "worker", *_COMMON))
    report.require(
        "primary/managed-dead/token-required",
        all(outcome.returncode == 75 for outcome in refused)
        and all((root / "registry/worker/agent.json").is_file()
                and not _state(root).get("closed_panes")
                for root in (case.python_root, case.rust_root)),
        f"managed-dead retirement did not require a token: {refused!r}",
    )
    tokens = [
        json.loads((root / "registry/worker/agent.json").read_text(encoding="utf-8"))["token"]
        for root in (case.python_root, case.rust_root)
    ]
    recovery_arguments = [_stop_recovery_arguments(outcome) for outcome in refused]
    report.require(
        "primary/managed-dead/exact-recovery-command",
        all(arguments is not None and "stop" in arguments and "worker" in arguments
            for arguments in recovery_arguments)
        and all(
            _option_values(arguments or (), "--registry") == ["<ROOT>/registry"]
            and _option_values(arguments or (), "--herdr-bin") == ["<ROOT>/fake-herdr"]
            and _option_values(arguments or (), "--expected-token") == [token]
            for arguments, token in zip(recovery_arguments, tokens, strict=True)
        ),
        f"recovery command lost its trusted token or selected globals: {refused!r}",
    )
    original_records: list[tuple[Path, bytes]] = []
    for root in (case.python_root, case.rust_root):
        path = root / "registry/worker/agent.json"
        original = path.read_bytes()
        original_records.append((path, original))
        document = json.loads(original)
        document["token"] += "-replacement"
        path.write_text(json.dumps(document), encoding="utf-8")
    stale_retries = [
        harness._invoke_one(command, root, arguments[1:])
        if arguments is not None else Outcome(1, "", "missing recovery command")
        for command, root, arguments in zip(
            (harness.python, harness.rust),
            (case.python_root, case.rust_root), recovery_arguments, strict=True,
        )
    ]
    report.require(
        "primary/managed-dead/stale-advice-preserves-replacement",
        all(outcome.returncode == 75 for outcome in stale_retries)
        and all(
            (arguments := _stop_recovery_arguments(outcome)) is not None
            and arguments[-1] == "doctor"
            and not _option_values(arguments, "--expected-token")
            for outcome in stale_retries
        )
        and all((root / "registry/worker/agent.json").is_file()
                and not _state(root).get("closed_panes")
                for root in (case.python_root, case.rust_root)),
        f"stale advice acquired a replacement generation: {stale_retries!r}",
    )
    for path, original in original_records:
        path.write_bytes(original)
    _change(case, {"fail_read": True})
    capture_failures = [
        harness._invoke_one(command, root, (
            "stop", "worker", "--expected-token", token, *_COMMON,
        ))
        for command, root, token in zip(
            (harness.python, harness.rust),
            (case.python_root, case.rust_root),
            tokens,
            strict=True,
        )
    ]
    report.require(
        "primary/managed-dead/capture-failure-is-busy",
        all(outcome.returncode == 75 for outcome in capture_failures)
        and all((root / "registry/worker/agent.json").is_file()
                for root in (case.python_root, case.rust_root)),
        f"managed-dead capture failures did not preserve delivery classification: {capture_failures!r}",
    )
    _change(case, {"fail_read": False, "fixture_shell_pid": 2_147_483_000})
    proof_failures = [
        harness._invoke_one(command, root, (
            "stop", "worker", "--expected-token", token, *_COMMON,
        ))
        for command, root, token in zip(
            (harness.python, harness.rust),
            (case.python_root, case.rust_root),
            tokens,
            strict=True,
        )
    ]
    report.require(
        "primary/managed-dead/procfs-failure-is-busy",
        all(outcome.returncode == 75 for outcome in proof_failures)
        and all((root / "registry/worker/agent.json").is_file()
                for root in (case.python_root, case.rust_root)),
        f"managed-dead procfs failures diverged: {proof_failures!r}",
    )
    _change(case, {
        "fixture_shell_pid": harness.fixture_shell.pid,
        "fail_process_info": True,
    })
    control_failures = [
        harness._invoke_one(command, root, (
            "stop", "worker", "--expected-token", token, *_COMMON,
        ))
        for command, root, token in zip(
            (harness.python, harness.rust),
            (case.python_root, case.rust_root),
            tokens,
            strict=True,
        )
    ]
    report.require(
        "primary/managed-dead/process-info-failure-is-busy",
        all(outcome.returncode == 75 for outcome in control_failures)
        and all((root / "registry/worker/agent.json").is_file()
                for root in (case.python_root, case.rust_root)),
        f"managed-dead process-info failures diverged: {control_failures!r}",
    )
    _change(case, {"fail_process_info": False})
    for field, value in (("mode", "headless"), ("backend", "tmux")):
        original_documents: list[tuple[Path, dict[str, object]]] = []
        for root in (case.python_root, case.rust_root):
            path = root / "registry/worker/agent.json"
            document = json.loads(path.read_text(encoding="utf-8"))
            original_documents.append((path, document.copy()))
            document[field] = value
            path.write_text(json.dumps(document), encoding="utf-8")
        malformed = [
            harness._invoke_one(command, root, (
                "stop", "worker", "--expected-token", token, *_COMMON,
            ))
            for command, root, token in zip(
                (harness.python, harness.rust),
                (case.python_root, case.rust_root),
                tokens,
                strict=True,
            )
        ]
        report.require(
            f"primary/managed-dead/malformed-{field}-refusal",
            all(outcome.returncode == 75 for outcome in malformed)
            and all(not _state(root).get("closed_panes")
                    for root in (case.python_root, case.rust_root)),
            f"malformed managed record was not refused consistently: {malformed!r}",
        )
        for path, document in original_documents:
            path.write_text(json.dumps(document), encoding="utf-8")
    for root in (case.python_root, case.rust_root):
        path = root / "registry/worker/agent.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        document["lifecycle"] = "stopping"
        path.write_text(json.dumps(document), encoding="utf-8")
    stopping = [
        harness._invoke_one(command, root, (
            "stop", "worker", "--expected-token", token, *_COMMON,
        ))
        for command, root, token in zip(
            (harness.python, harness.rust),
            (case.python_root, case.rust_root),
            tokens,
            strict=True,
        )
    ]
    report.require(
        "primary/managed-dead/stopping-without-proof-refusal",
        all(outcome.returncode == 75 for outcome in stopping)
        and all(not _state(root).get("closed_panes")
                for root in (case.python_root, case.rust_root)),
        f"unproved stopping generation was retired: {stopping!r}",
    )
    for root in (case.python_root, case.rust_root):
        path = root / "registry/worker/agent.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        document["lifecycle"] = "running"
        path.write_text(json.dumps(document), encoding="utf-8")
    outcomes = []
    for command, root, arguments in zip(
        (harness.python, harness.rust), (case.python_root, case.rust_root),
        recovery_arguments, strict=True,
    ):
        outcomes.append(harness._invoke_one(command, root, arguments[1:])
                        if arguments is not None else Outcome(1, "", "missing recovery command"))
    report.require(
        "primary/managed-dead/recovery-parity",
        all(outcome.returncode == 0 for outcome in outcomes)
        and _json(outcomes[0]) == _json(outcomes[1])
        and all(
            _state(root).get("closed_panes") == ["w1:p1"]
            and not (root / "registry/worker").exists()
            and len(list((root / "registry/archive").glob("*/output.json"))) == 1
            for root in (case.python_root, case.rust_root)
        ),
        f"managed-dead retirement diverged: {outcomes!r}",
    )


def _stop_recovery_quoted_globals(harness: Harness, report: Report) -> None:
    """Execute refusal advice with literal spaces, quotes, and shell characters."""
    case = harness.case("primary-stop-quoted-globals")
    registry = "private state's $(literal); registry"
    executable = "fake herdr's $(literal); executable"
    common = (f"--registry=<ROOT>/{registry}", f"--herdr-bin=<ROOT>/{executable}")
    for root in (case.python_root, case.rust_root):
        shutil.copyfile(root / "fake-herdr", root / executable)
        (root / executable).chmod(0o700)
    outcomes = harness.invoke(case, (
        "start", "worker", "--cwd", "<ROOT>", "--workspace-id", "w1", *common,
    ))
    report.require("primary/stop/quoted-globals/start", all(outcome.returncode == 0 for outcome in outcomes),
                   f"fixture could not start with literal globals: {outcomes!r}")
    if any(outcome.returncode != 0 for outcome in outcomes):
        return
    _change(case, {"empty_shell": True, "sessionless": True})
    refused = harness.invoke(case, ("stop", "worker", *common))
    commands = [_stop_recovery_arguments(outcome) for outcome in refused]
    tokens = [
        json.loads((root / registry / "worker/agent.json").read_text(encoding="utf-8"))["token"]
        for root in (case.python_root, case.rust_root)
    ]
    report.require(
        "primary/stop/quoted-globals/advice",
        all(outcome.returncode == 75 for outcome in refused)
        and all(
            arguments is not None
            and _option_values(arguments, "--registry") == [f"<ROOT>/{registry}"]
            and _option_values(arguments, "--herdr-bin") == [f"<ROOT>/{executable}"]
            and _option_values(arguments, "--expected-token") == [token]
            for arguments, token in zip(commands, tokens, strict=True)
        ),
        f"quoted recovery advice changed a literal value: {refused!r}",
    )
    recovered = [
        harness._invoke_one(command, root, arguments[1:])
        if arguments is not None else Outcome(1, "", "missing recovery command")
        for command, root, arguments in zip(
            (harness.python, harness.rust), (case.python_root, case.rust_root), commands, strict=True,
        )
    ]
    report.require(
        "primary/stop/quoted-globals/execute",
        all(outcome.returncode == 0 for outcome in recovered)
        and all(not (root / registry / "worker").exists()
                and _state(root).get("closed_panes") == ["w1:p1"]
                and len(list((root / registry / "archive").glob("*/agent.json"))) == 1
                for root in (case.python_root, case.rust_root)),
        f"printed advice could not recover the selected registry: {recovered!r}",
    )


def _adoption(harness: Harness, report: Report) -> None:
    case = harness.case("primary-adoption")
    common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
    python, _ = _pair(harness, report, case, "primary/adopt/register", (
        "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
        "--cwd", "<ROOT>", "--harness", "codex", "--session", "session-1", *common,
    ))
    adopted = _json(python)
    report.require("primary/adopt/metadata", isinstance(adopted, dict)
                   and all(adopted.get(key) == value for key, value in {
                       "name": "foreign", "adapter": "herdr-foreign", "mode": "interactive",
                       "backend": "herdr", "pane_id": "w1:p1", "tab_id": "w1:t1",
                       "workspace_id": "w1", "session_agent": "codex",
                       "session_value": "session-1", "capabilities": _ADOPTED_CAPABILITIES,
                   }.items()), f"adoption lost exact live identity: {adopted!r}")
    report.require(
        "primary/adopt/shell-identity-schema",
        isinstance(adopted, dict)
        and _valid_foreign_shell_identity(adopted.get("foreign_shell_identity")),
        f"adoption did not retain the exact six-field shell identity: {adopted!r}",
    )
    for command in (("status", "foreign"), ("list",),
                    ("wait", "foreign", "--timeout", "0")):
        _pair(harness, report, case, f"primary/adopt/{command[0]}", (*command, *common))
    _pair(harness, report, case, "primary/adopt/send",
          ("send", "foreign", "retained request", "--message-id", "adopted-1", *common))
    _pair(harness, report, case, "primary/adopt/read", ("read", "foreign", *common))
    _pair(harness, report, case, "primary/adopt/bind",
          ("bind-session", "foreign", "session-1", *_GOAL_COMMAND, *common))
    _pair(harness, report, case, "primary/adopt/pause", ("pause", "foreign", *common))
    _pair(harness, report, case, "primary/adopt/resume", ("resume", "foreign", *common))
    _pair(harness, report, case, "primary/adopt/attach", ("attach", "foreign", *common))
    python, _ = _pair(harness, report, case, "primary/adopt/unregister",
                      ("stop", "foreign", *common))
    stopped = _json(python)
    report.require("primary/adopt/runtime-preserved", isinstance(stopped, dict)
                   and stopped.get("runtime_preserved") is True
                   and stopped.get("pane_closed") is False
                   and stopped.get("tab_closed") is False
                   and all(not _state(root).get("closed")
                           and not _state(root).get("closed_panes")
                           and not (root / "registry/foreign").exists()
                           and len(list((root / "registry/archive").glob("*/agent.json"))) == 1
                           and len(list((root / "registry/archive").glob(
                               "*/queue/processed/adopted-1.json"))) == 1
                           for root in (case.python_root, case.rust_root)),
                   f"unregister mutated a foreign runtime or lost durable state: {stopped!r}")

    retirement_cases: tuple[tuple[str, Mapping[str, object], int], ...] = (
        ("absent-idle", {"empty_shell": True}, 0),
        (
            "absent-restart-during-capture",
            {"empty_shell": True, "restart_agent_on_read": True},
            75,
        ),
        (
            "absent-leaves-idle-during-capture",
            {"empty_shell": True, "leave_idle_shell_on_read": True},
            75,
        ),
        ("live-moved-tab", {"tab_id": "w1:moved"}, 0),
        ("absent-moved-tab", {"empty_shell": True, "tab_id": "w1:moved"}, 75),
    )
    for label, changed_state, expected in retirement_cases:
        retirement = harness.case(f"primary-adoption-retire-{label}")
        _pair(harness, report, retirement, f"primary/adopt/{label}/register", (
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "codex", "--session", "session-1",
            *common,
        ))
        _change(retirement, changed_state)
        _pair(
            harness,
            report,
            retirement,
            f"primary/adopt/{label}/stop",
            ("stop", "foreign", *common),
            expected,
        )
        report.require(
            f"primary/adopt/{label}/state",
            all(
                not _state(root).get("closed")
                and not _state(root).get("closed_panes")
                and (root / "registry/foreign").exists() == (expected != 0)
                for root in (retirement.python_root, retirement.rust_root)
            ),
            f"foreign runtime mutation or registry outcome diverged for {label}",
        )

    replacement_shell = {
        "fixture_shell_pid": harness.replacement_shell.pid,
        "fixture_shell_executable": harness.fixture_shell_executable,
    }
    for label, initial_state, session_arguments in (
        ("live-replaced-shell", {}, ("--session", "session-1")),
        ("sessionless-live-replaced-shell", {"sessionless": True}, ()),
    ):
        replacement_case = harness.case(f"primary-adoption-retire-{label}", initial_state)
        _pair(harness, report, replacement_case, f"primary/adopt/{label}/register", (
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "codex", *session_arguments, *common,
        ))
        _change(replacement_case, replacement_shell)
        _pair(
            harness,
            report,
            replacement_case,
            f"primary/adopt/{label}/stop",
            ("stop", "foreign", *common),
            75,
        )
        report.require(
            f"primary/adopt/{label}/preserved",
            all(
                (root / "registry/foreign/agent.json").is_file()
                and not _state(root).get("closed")
                and not _state(root).get("closed_panes")
                for root in (replacement_case.python_root, replacement_case.rust_root)
            ),
            f"live replacement was archived or its runtime was mutated for {label}",
        )

    legacy_live = harness.case("primary-adoption-retire-live-legacy")
    _pair(harness, report, legacy_live, "primary/adopt/live-legacy/register", (
        "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
        "--cwd", "<ROOT>", "--harness", "codex", "--session", "session-1", *common,
    ))
    for root in (legacy_live.python_root, legacy_live.rust_root):
        path = root / "registry/foreign/agent.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        del document["foreign_shell_identity"]
        path.write_text(json.dumps(document), encoding="utf-8")
    _pair(
        harness,
        report,
        legacy_live,
        "primary/adopt/live-legacy/stop",
        ("stop", "foreign", *common),
        75,
    )
    report.require(
        "primary/adopt/live-legacy/preserved",
        all(
            (root / "registry/foreign/agent.json").is_file()
            and not _state(root).get("closed")
            and not _state(root).get("closed_panes")
            for root in (legacy_live.python_root, legacy_live.rust_root)
        ),
        "live legacy record was automatically retired or its runtime was mutated",
    )

    mismatch_cases: tuple[tuple[str, Mapping[str, object], tuple[str, ...]], ...] = (
        ("non-agent", {"harness": None}, ()),
        ("harness", {}, ("--harness", "claude")),
        ("workspace", {}, ("--workspace", "wrong")),
        ("session", {}, ("--session", "wrong-session")),
    )
    for label, state, replacement in mismatch_cases:
        mismatch = harness.case(f"primary-adoption-{label}", state)
        arguments = [
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "codex", *common,
        ]
        option = replacement[0] if replacement else None
        if option is not None:
            index = arguments.index(option) if option in arguments else -1
            if index >= 0:
                arguments[index:index + 2] = replacement
            else:
                arguments.extend(replacement)
        outcomes = harness.invoke(mismatch, arguments)
        report.require(f"primary/adopt/refuse-{label}", all(
            outcome.returncode == 75 for outcome in outcomes
        ) and all(not (root / "registry/foreign").exists()
                  and not _state(root).get("closed")
                  for root in (mismatch.python_root, mismatch.rust_root)),
            f"identity mismatch was adopted or mutated: {outcomes!r}")

    for label, producer, consumer in (
        ("python-rust", harness.python, harness.rust),
        ("rust-python", harness.rust, harness.python),
    ):
        interop = harness.case(f"primary-adoption-interop-{label}")
        root = interop.python_root
        adopted = harness._invoke_one(producer, root, (
            "adopt", "foreign", "--pane", "w1:p1", "--workspace", "project",
            "--cwd", "<ROOT>", "--harness", "codex", *common,
        ))
        status = harness._invoke_one(consumer, root, ("status", "foreign", *common))
        sent = harness._invoke_one(consumer, root, (
            "send", "foreign", "cross-engine adopted request", *common,
        ))
        _change(interop, {"empty_shell": True})
        stopped = harness._invoke_one(consumer, root, ("stop", "foreign", *common))
        report.require(f"primary/adopt/interop-{label}", all(
            outcome.returncode == 0 for outcome in (adopted, status, sent, stopped)
        ) and _state(root).get("submitted") == ["cross-engine adopted request"]
            and not _state(root).get("closed") and not _state(root).get("closed_panes")
            and len(list((root / "registry/archive").glob("*/agent.json"))) == 1,
            f"adopted record was not safely portable between editions: "
            f"{(adopted, status, sent, stopped)!r}")


def _outcome_field(outcome: Outcome) -> object:
    value = _json(outcome)
    return value.get("outcome") if isinstance(value, dict) else None


def _identity_and_rename(harness: Harness, report: Report) -> None:
    """Anchors, per-effect identity checks, rename, doctor, and cross-edition rename recovery."""
    common = ("--herdr-bin", "<HERDR>", "--registry", "<ROOT>/registry")
    case = harness.case("identity-rename")
    if not _start(harness, report, case, "identity/start", "--harness", "claude"):
        return
    python, _ = _pair(harness, report, case, "identity/status", ("status", "worker", *common))
    status = _json(python)
    report.require("identity/anchored", isinstance(status, dict)
                   and status.get("terminal_id") == "term-1"
                   and isinstance(status.get("harness_identity"), dict)
                   and status["harness_identity"].get("pid") == "<PID>"
                   and status["harness_identity"].get("version") == 1,
                   f"start did not pin terminal and harness: {status!r}")
    _pair(harness, report, case, "identity/doctor-clean", ("doctor", *common))
    _pair(harness, report, case, "identity/rename", ("rename", "worker", "reviewer", *common))
    report.require("identity/rename-presentation", all(
        _state(root).get("name") == "reviewer" and _state(root).get("tab_label") == "reviewer"
        and (root / "registry" / "reviewer" / "agent.json").exists()
        and not (root / "registry" / "worker").exists()
        for root in (case.python_root, case.rust_root)), "rename left a stale name or label")
    _pair(harness, report, case, "identity/send-renamed", ("send", "reviewer", "after rename", *common))
    report.require("identity/send-renamed-delivered", all(
        _submission_count(root, "after rename") == 1 for root in (case.python_root, case.rust_root)),
        "send after rename was not delivered exactly once")
    _change(case, {"input_expect": True})
    _pair(harness, report, case, "identity/send-expect", ("send", "reviewer", "with expect", *common))
    pastes = [[call for call in cast(list[list[str]], _state(root)["calls"])
               if call[:2] == ["pane", "send-text"]] for root in (case.python_root, case.rust_root)]
    report.require("identity/expect-flag-used", all(
        calls and calls[-1][:4] == ["pane", "send-text", "--expect-terminal", "term-1"]
        for calls in pastes), f"an advertised input-expect capability was not used: {pastes!r}")
    _change(case, {"input_expect": False, "tab_label": "retitled"})
    outcomes = harness.invoke(case, ("send", "reviewer", "after drift", *common))
    report.require("identity/label-drift-refused", all(
        outcome.returncode == 75 and _outcome_field(outcome) == "pending" for outcome in outcomes)
        and all(_submission_count(root, "after drift") == 0
                for root in (case.python_root, case.rust_root)),
        f"label drift did not keep the message pending: {outcomes!r}")
    outcomes = harness.invoke(case, ("doctor", *common))
    findings = [_json(outcome) for outcome in outcomes]
    report.require("identity/doctor-drift", all(outcome.returncode == 1 for outcome in outcomes)
                   and findings[0] == findings[1], f"doctor disagreed on drift: {outcomes!r}")
    _pair(harness, report, case, "identity/doctor-repair", ("doctor", "--repair-labels", *common))
    report.require("identity/repaired-label", all(
        _state(root).get("tab_label") == "reviewer" for root in (case.python_root, case.rust_root)),
        "label repair did not restore the label")
    # An interrupted rename by one edition is finished by the other, on the same registry.
    for first, second, root in ((harness.python, harness.rust, case.python_root),
                                (harness.rust, harness.python, case.rust_root)):
        state = _state(root)
        state["fail_tab_rename_once"] = True
        (root / "state.json").write_text(json.dumps(state), encoding="utf-8")
        crashed = harness._invoke_one(first, root, ("rename", "reviewer", "lead", *common))
        blocked = harness._invoke_one(second, root, ("send", "lead", "blocked", *common))
        finished = harness._invoke_one(second, root, ("rename", "reviewer", "lead", *common))
        record = json.loads((root / "registry" / "lead" / "agent.json").read_text(encoding="utf-8")) \
            if (root / "registry" / "lead" / "agent.json").exists() else {}
        history = record.get("name_history", []) if isinstance(record, dict) else []
        report.require(f"identity/cross-recovery/{root.parent.name}",
                       crashed.returncode != 0 and blocked.returncode != 0
                       and finished.returncode == 0
                       and [entry.get("name") for entry in history] == ["worker", "reviewer"]
                       and not list((root / "registry" / ".renames").glob("*.json"))
                       and _state(root).get("tab_label") == "lead",
                       f"crashed={crashed!r} blocked={blocked!r} finished={finished!r} history={history!r}")
    _retire_fixture_processes(case)


def _invalid_cli(harness: Harness, report: Report) -> None:
    case = harness.case("primary-invalid-cli")
    for label, arguments in (
        ("unknown", ("unknown", *FIXTURE_HERDR)), ("missing-name", ("status", *FIXTURE_HERDR)),
        ("missing-message", ("send", "worker", *FIXTURE_HERDR)),
        ("empty-message", ("send", "worker", "", *FIXTURE_HERDR)),
        ("blank-message", ("send", "worker", " \n\t", *FIXTURE_HERDR)),
        ("empty-goal", ("goal", "worker", "", *FIXTURE_HERDR)),
        ("abbreviation", ("start", "worker", "--work", "w1", *FIXTURE_HERDR)),
        ("nonfinite", ("send", "worker", "text", "--ready-timeout", "nan", *FIXTURE_HERDR)),
        ("zero-working", ("send", "worker", "text", "--working-timeout", "0", *FIXTURE_HERDR)),
        ("zero-startup", ("start", "worker", "--startup-timeout", "0", *FIXTURE_HERDR)),
        ("environment-missing-equals", ("start", "worker", "--env", "MISSING_EQUALS", *FIXTURE_HERDR)),
        ("environment-empty-name", ("start", "worker", "--env", "=value", *FIXTURE_HERDR)),
        ("environment-invalid-name", ("start", "worker", "--env", "BAD-NAME=value", *FIXTURE_HERDR)),
        ("zero-lines", ("read", "worker", "--lines", "0", *FIXTURE_HERDR)),
        ("underscore-count", ("read", "worker", "--lines", "1_0", *FIXTURE_HERDR)),
        ("unicode-time", ("wait", "worker", "--timeout", "١.0", *FIXTURE_HERDR)),
        ("prompt-conflict", ("send", "worker", "text", "--file", "file.txt", *FIXTURE_HERDR)),
        ("bad-goal-command", ("goal", "worker", "--goal-command-json", "[]", *FIXTURE_HERDR)),
        ("missing-global-value", ("--registry", "--help")),
        ("wrong-command-option", ("status", "worker", "--brief", "text", *FIXTURE_HERDR)),
        ("legacy-recovery-missing-token", (
            "stop", "worker", "--recover-legacy-adoption",
            "--expected-record-sha256", "0" * 64, *FIXTURE_HERDR,
        )),
        ("legacy-recovery-missing-hash", (
            "stop", "worker", "--recover-legacy-adoption",
            "--expected-token", "generation", *FIXTURE_HERDR,
        )),
        ("legacy-hash-without-recovery", (
            "stop", "worker", "--expected-record-sha256", "0" * 64, *FIXTURE_HERDR,
        )),
        ("stop-missing-name", (*_COMMON, "stop")),
        ("stop-unknown-option", (*_COMMON, "stop", "worker", "--unrecognized")),
        ("stop-missing-option-value", (*_COMMON, "stop", "worker", "--expected-token")),
    ):
        outcomes = harness.invoke(case, arguments)
        report.require(f"primary/cli/{label}", all(outcome.returncode == 2
                       and "traceback" not in outcome.stderr.lower() and "panicked" not in outcome.stderr.lower()
                       for outcome in outcomes), f"invalid invocation was not a clean usage error: {outcomes!r}")
        if arguments and (arguments[0] == "stop" or label.startswith("stop-")):
            report.require(f"primary/cli/{label}/recovery-command",
                           all(_stop_recovery_arguments(outcome) is not None for outcome in outcomes),
                           f"stop usage refusal omitted recovery advice: {outcomes!r}")
        if label.startswith("stop-"):
            report.require(
                f"primary/cli/{label}/recovery-globals",
                all(
                    (recovery := _stop_recovery_arguments(outcome)) is not None
                    and recovery[-1] == "doctor"
                    and _option_values(recovery, "--registry") == ["<ROOT>/registry"]
                    and _option_values(recovery, "--herdr-bin") == ["<ROOT>/fake-herdr"]
                    for outcome in outcomes
                ),
                f"stop syntax advice lost parsed globals or inferred a target: {outcomes!r}",
            )
    report.require("primary/cli/no-registry-side-effect", all(not (root / ".agentctl").exists()
                   for root in (case.python_root, case.rust_root)), "invalid syntax created persistent state")


def build_report(python_command: Sequence[str], rust_command: Sequence[str]) -> Report:
    """Compare the canonical interface through independent executable clients."""
    report = Report()
    with tempfile.TemporaryDirectory(prefix="agentctl-cross-") as temporary:
        harness = Harness(Path(temporary), python_command, rust_command)
        try:
            _orientation(harness, report)
            _lifecycle(harness, report)
            _conversations(harness, report)
            _profiles(harness, report)
            _profile_resume(harness, report)
            _workspace_policy(harness, report)
            _skill_install(harness, report)
            _profile_refusals(harness, report)
            _herdr_093_compatibility(harness, report)
            _muse_adoption(harness, report)
            _muse14_status(harness, report)
            _handoff_and_pending(harness, report)
            _registry_and_interop(harness, report)
            _legacy_adopted_registry_and_queue(harness, report)
            _ownership(harness, report)
            _dead_adopted_retirement(harness, report)
            _managed_dead_recovery(harness, report)
            _stop_recovery_quoted_globals(harness, report)
            _revive_interop(harness, report)
            _adoption(harness, report)
            _identity_and_rename(harness, report)
            # Imported here: the group's module imports this one's helpers.
            from agentctl_nested_records import nested_records
            nested_records(harness, report)
            _invalid_cli(harness, report)
            harness.require_no_host_cli(report)
        finally:
            harness.close()
    return report


def compare_agentctl(python_command: Sequence[str], rust_command: Sequence[str]) -> int:
    """Report semantic differences and return a conventional process exit code."""
    report = build_report(python_command, rust_command)
    for failure in report.failures:
        print(f"DIVERGENCE [{failure}]")
    if report.failures:
        print(f"cross[agentctl]: {len(report.failures)} divergence(s) out of {report.checks} paired checks")
        return 1
    print(f"cross[agentctl]: OK - {report.checks} paired checks agree")
    return 0

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import time
from dataclasses import replace
from pathlib import Path

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location(
    "integration_tip_validate",
    ROOT / "ci-hub" / "health" / "integration_tip_validate.py",
)
driver = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = driver
SPEC.loader.exec_module(driver)

SHA = "1234567890abcdef1234567890abcdef12345678"


def settings(tmp_path: Path) -> object:
    source = tmp_path / "hermit"
    source.mkdir()
    operational_tool = tmp_path / "operational-tool"
    operational_tool.write_text("#!/bin/sh\nexit 0\n")
    operational_tool.chmod(0o755)
    return driver.Settings.defaults(
        tool_root=ROOT,
        state_root=tmp_path,
        source_checkout=source,
        ci_hub=tmp_path / "ci-hub",
        operational_tool=operational_tool,
        tool_parent_sha="f" * 40,
        state_dir=tmp_path / "tick-state",
    )


class Authorities:
    def __init__(
        self,
        *,
        records: int = 0,
        in_progress: int = 0,
        admissible: bool = True,
    ) -> None:
        self.records = records
        self.record_verdict = "VALIDATED"
        self.record_result = "pass"
        self.record_exit_code = 0
        self.record_detail = "all required validation gates passed"
        self.in_progress = in_progress
        self.admissible = admissible
        self.launches: list[str] = []

    def fetch(self, _settings: object, _timeout: float) -> str:
        return SHA

    def run(
        self, command: list[str] | tuple[str, ...], _cwd: Path, _timeout: float
    ) -> subprocess.CompletedProcess[str]:
        if "validate-status" in command and "--sha" in command:
            in_progress = [
                {"sha": SHA, "in_progress": True, "summary": f"run {index}"}
                for index in range(self.in_progress)
            ]
            verdict = self.record_verdict if self.records else "NOT-VALIDATED"
            exit_code = self.record_exit_code if self.records else 4
            exact_records = [
                {
                    "verdict": self.record_verdict,
                    "result": self.record_result,
                    "exit_code": self.record_exit_code,
                    "started_at": "2026-09-05T00:00:00Z",
                    "finished_at": "2026-09-05T00:10:00Z",
                    "detail": self.record_detail,
                }
                for _ in range(self.records)
            ]
            report = {
                "schema_version": driver.EXACT_VALIDATE_STATUS_SCHEMA_VERSION,
                "repo": driver.REPO,
                "sha": SHA,
                "verdict": verdict,
                "exit_code": exit_code,
                "qualifying_count": self.records if verdict == "VALIDATED" else 0,
                "disqualified_count": 0 if verdict == "VALIDATED" else self.records,
                "record_count": self.records,
                "exact_records": exact_records,
                "ledger_freshness": (
                    None
                    if self.records
                    else {
                        "state": "current",
                        "local_absence_is_authoritative": True,
                    }
                ),
                "in_progress_count": len(in_progress),
                "in_progress": in_progress,
                "run_handles": in_progress,
                "unreadable_handles": [],
            }
            return subprocess.CompletedProcess(command, report["exit_code"], json.dumps(report), "")
        if "admission-status" in command:
            assert "--target-was-fetched-main" in command
            report = {
                "schema_version": 1,
                "target": SHA,
                "kind": "validate",
                "admissible": self.admissible,
                "state": "admissible" if self.admissible else "busy",
                "reason_code": (
                    "immediate-admission-available"
                    if self.admissible
                    else "global-priority-queue-not-ready"
                ),
                "detail": (
                    "validation slot 2 allows an immediate attempt"
                    if self.admissible
                    else "global priority queue position 2 is behind another validate"
                ),
            }
            return subprocess.CompletedProcess(
                command, 0 if self.admissible else 1, json.dumps(report), ""
            )
        raise AssertionError(command)

    def spawn(
        self,
        _settings: object,
        target: str,
        attempt_id: str,
        _unit: str,
        _validate_log: Path,
        _launcher_log: Path,
        _timeout: float,
    ) -> dict[str, object]:
        assert target == SHA
        self.launches.append(attempt_id)
        return {"pid": 123, "start_ticks": 456, "boot_id": "boot-fixture"}


def read_twice(
    tmp_path: Path,
    authorities: Authorities,
    *,
    launcher_active=lambda _settings, _attempt, _timeout, _run: False,
):
    configured = settings(tmp_path)
    first = driver.run_tick(
        configured,
        fetch=authorities.fetch,
        run=authorities.run,
        spawn=authorities.spawn,
        launcher_active=launcher_active,
    )
    time.sleep(0.01)
    second = driver.run_tick(
        configured,
        fetch=authorities.fetch,
        run=authorities.run,
        spawn=authorities.spawn,
        launcher_active=launcher_active,
    )
    return first, second


def test_two_record_present_readings_launch_nothing(tmp_path: Path, capsys) -> None:
    authorities = Authorities(records=1)
    assert read_twice(tmp_path, authorities) == (0, 0)
    assert authorities.launches == []
    output = capsys.readouterr().out
    assert output.count("state=record-present") == 2
    assert output.count("record_count=1") == 2


def test_two_in_progress_readings_launch_nothing(tmp_path: Path, capsys) -> None:
    authorities = Authorities(in_progress=1)

    def launcher_must_not_be_queried(*_args) -> bool:
        raise AssertionError("canonical in-progress authority must short-circuit systemd")

    assert read_twice(
        tmp_path, authorities, launcher_active=launcher_must_not_be_queried
    ) == (0, 0)
    assert authorities.launches == []
    output = capsys.readouterr().out
    assert output.count("state=in-progress") == 2
    assert output.count("in_progress_count=1") == 2


def test_absent_record_and_run_launches_exactly_once_across_two_readings(
    tmp_path: Path, capsys
) -> None:
    authorities = Authorities()
    assert read_twice(
        tmp_path,
        authorities,
        launcher_active=lambda _settings, _attempt, _timeout, _run: True,
    ) == (0, 0)
    assert len(authorities.launches) == 1
    output = capsys.readouterr().out
    assert output.count("state=launched") == 1
    assert output.count("launched=1") == 1
    assert output.count("no second validate started") == 1


def test_failed_prior_launch_is_not_reported_as_never_attempted(
    tmp_path: Path, capsys
) -> None:
    configured = settings(tmp_path)
    driver.atomic_write_json(
        configured.state_dir / "latest.json",
        {
            "schema_version": 1,
            "attempt_id": "prior-attempt",
            "target": SHA,
            "state": "launcher-running",
            "started_at": "2026-09-05T01:00:00+00:00",
            "launcher_process": {
                "pid": 123,
                "start_ticks": 456,
                "boot_id": "old-boot",
            },
        },
    )
    authorities = Authorities()
    assert (
        driver.run_tick(
            configured,
            fetch=authorities.fetch,
            run=authorities.run,
            spawn=authorities.spawn,
            launcher_active=lambda _settings, _attempt, _timeout, _run: False,
        )
        == 0
    )
    assert len(authorities.launches) == 1
    output = capsys.readouterr().out
    assert "prior attempt prior-attempt ended or stopped being observable" in output
    assert "and no ledger row" in output
    assert "never attempted" not in output


def test_no_prior_handle_does_not_claim_never_attempted(tmp_path: Path, capsys) -> None:
    authorities = Authorities()
    assert (
        driver.run_tick(
            settings(tmp_path),
            fetch=authorities.fetch,
            run=authorities.run,
            spawn=authorities.spawn,
        )
        == 0
    )
    output = capsys.readouterr().out
    assert "ledger absence alone cannot tell" in output
    assert "never attempted or ended before writing a ledger row" in output


def test_failed_run_handle_is_reported_as_a_prior_attempt(tmp_path: Path, capsys) -> None:
    authorities = Authorities()

    def failed_handle(command, cwd, timeout):
        result = authorities.run(command, cwd, timeout)
        if "validate-status" in command:
            value = json.loads(result.stdout)
            value["run_handles"] = [
                {
                    "unit": "validate-failed.service",
                    "state": "completed",
                    "sha": SHA,
                    "started_at": "2026-09-05T01:00:00Z",
                    "finished_at": "2026-09-05T01:05:00Z",
                    "result": "failure",
                    "exit_code": 1,
                }
            ]
            return subprocess.CompletedProcess(command, result.returncode, json.dumps(value), "")
        return result

    assert (
        driver.run_tick(
            settings(tmp_path),
            fetch=authorities.fetch,
            run=failed_handle,
            spawn=authorities.spawn,
        )
        == 0
    )
    output = capsys.readouterr().out
    assert "prior attempt validate-failed.service ended or stopped being observable" in output
    assert "state=completed result=failure exit=1 and no ledger row" in output


def test_unavailable_queue_refuses_without_starting_a_launcher(
    tmp_path: Path, capsys
) -> None:
    authorities = Authorities(admissible=False)
    assert read_twice(tmp_path, authorities) == (0, 0)
    assert authorities.launches == []
    output = capsys.readouterr().out
    assert output.count("state=busy") == 2
    assert output.count("no validate started") == 2


def test_launch_failure_is_recorded_without_a_ledger_claim(tmp_path: Path, capsys) -> None:
    authorities = Authorities()
    configured = settings(tmp_path)

    def fail(*_args: object) -> dict[str, object]:
        latest = json.loads((configured.state_dir / "latest.json").read_text())
        assert latest["state"] == "starting"
        assert latest["launcher_unit"].endswith("-launcher.service")
        raise OSError("fixture launcher refused")

    assert (
        driver.run_tick(
            configured,
            fetch=authorities.fetch,
            run=authorities.run,
            spawn=fail,
        )
        == 1
    )
    latest = json.loads((configured.state_dir / "latest.json").read_text())
    assert latest["state"] == "launch-failed"
    output = capsys.readouterr().out
    assert "failed before a ledger row was written" in output
    assert "state=launch-failed" in output


def test_pre_recorded_launcher_unit_closes_the_post_accept_kill_window(
    tmp_path: Path, capsys
) -> None:
    configured = settings(tmp_path)
    driver.atomic_write_json(
        configured.state_dir / "latest.json",
        {
            "schema_version": 1,
            "attempt_id": "interrupted-after-systemd-accept",
            "target": SHA,
            "state": "starting",
            "started_at": "2026-09-05T01:00:00+00:00",
            "launcher_unit": "validate-ops-tick-fixture-launcher.service",
        },
    )
    authorities = Authorities()
    assert (
        driver.run_tick(
            configured,
            fetch=authorities.fetch,
            run=authorities.run,
            spawn=authorities.spawn,
            launcher_active=lambda _settings, attempt, _timeout, _run: (
                attempt["launcher_unit"]
                == "validate-ops-tick-fixture-launcher.service"
            ),
        )
        == 0
    )
    assert authorities.launches == []
    assert "no second validate started" in capsys.readouterr().out


def test_launcher_runs_in_its_own_bounded_user_unit(tmp_path: Path, monkeypatch) -> None:
    configured = settings(tmp_path)
    commands: list[list[str]] = []

    def accepted(command, cwd, timeout):
        commands.append(list(command))
        assert cwd == tmp_path
        assert timeout == 7.0
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(driver, "run_bounded", accepted)
    result = driver.spawn_validate(
        configured,
        SHA,
        "attempt",
        "validate-ops-tick-fixture",
        tmp_path / "validate.log",
        tmp_path / "launcher.log",
        7.0,
    )
    command = commands[0]
    assert command[:4] == ["systemd-run", "--user", "--quiet", "--collect"]
    assert "--unit=validate-ops-tick-fixture-launcher" in command
    assert "--property=RuntimeMaxSec=5000" in command
    separator = command.index("--")
    assert command[separator + 1] == str(configured.operational_tool)
    assert command[separator + 2 : separator + 4] == [
        "--run-current-tool",
        "--state-root",
    ]
    assert "--tool-parent-sha" in command[separator:]
    assert command[command.index("--tool-parent-sha") + 1] == "f" * 40
    inner = command.index("--", separator + 1)
    assert command[inner + 1] == "validate-run"
    assert "--no-wait" in command[inner:]
    assert "--skip-if-recorded" in command[inner:]
    assert result["unit"] == "validate-ops-tick-fixture-launcher.service"


def test_launcher_refuses_a_short_lived_tool_root(tmp_path: Path) -> None:
    configured = settings(tmp_path)
    configured = replace(configured, operational_tool=None)
    with pytest.raises(driver.ObservationUnavailable, match="short-lived tool snapshot"):
        driver.spawn_validate(
            configured,
            SHA,
            "attempt",
            "validate-ops-tick-fixture",
            tmp_path / "validate.log",
            tmp_path / "launcher.log",
            7.0,
        )


@pytest.mark.parametrize(
    ("load_state", "active_state", "invocation", "expected"),
    [
        ("loaded", "activating", "invocation-1", True),
        ("loaded", "active", "invocation-1", True),
        ("loaded", "deactivating", "invocation-1", True),
        ("loaded", "failed", "invocation-1", False),
        ("not-found", "inactive", "", False),
    ],
)
def test_launcher_activity_comes_from_its_systemd_unit(
    tmp_path: Path,
    load_state: str,
    active_state: str,
    invocation: str,
    expected: bool,
) -> None:
    configured = settings(tmp_path)
    attempt = {"launcher_unit": "validate-ops-tick-fixture-launcher.service"}

    def systemctl(command, _cwd, _timeout):
        assert command[:3] == ["systemctl", "--user", "show"]
        output = (
            f"LoadState={load_state}\n"
            f"ActiveState={active_state}\n"
            "SubState=running\n"
            f"InvocationID={invocation}\n"
        )
        return subprocess.CompletedProcess(command, 0, output, "")

    assert driver.launcher_unit_is_active(configured, attempt, 1.0, systemctl) is expected


def test_unknown_admission_is_visible_and_never_launches(tmp_path: Path) -> None:
    authorities = Authorities()

    def unknown(command, cwd, timeout):
        if "admission-status" not in command:
            return authorities.run(command, cwd, timeout)
        report = {
            "schema_version": 1,
            "target": SHA,
            "kind": "validate",
            "admissible": False,
            "state": "unknown",
            "reason_code": "memory-admission-unknown",
            "detail": "cannot read MemAvailable",
        }
        return subprocess.CompletedProcess(command, 2, json.dumps(report), "")

    assert (
        driver.run_tick(
            settings(tmp_path),
            fetch=authorities.fetch,
            run=unknown,
            spawn=authorities.spawn,
        )
        == 2
    )
    assert authorities.launches == []


@pytest.mark.parametrize("state", ["stale", "quarantined", "refused"])
def test_non_busy_admission_refusals_stay_visible(
    tmp_path: Path, state: str
) -> None:
    authorities = Authorities()

    def refused(command, cwd, timeout):
        if "admission-status" not in command:
            return authorities.run(command, cwd, timeout)
        report = {
            "schema_version": 1,
            "target": SHA,
            "kind": "validate",
            "admissible": False,
            "state": state,
            "reason_code": f"fixture-{state}",
            "detail": f"fixture {state} refusal",
        }
        return subprocess.CompletedProcess(command, 3, json.dumps(report), "")

    assert (
        driver.run_tick(
            settings(tmp_path),
            fetch=authorities.fetch,
            run=refused,
            spawn=authorities.spawn,
        )
        == 2
    )
    assert authorities.launches == []


def test_unattributable_unreadable_handle_refuses_the_decision(tmp_path: Path) -> None:
    authorities = Authorities()

    def unreadable(command, cwd, timeout):
        result = authorities.run(command, cwd, timeout)
        if "validate-status" in command:
            value = json.loads(result.stdout)
            value["unreadable_handles"] = [
                {"path": "/state/broken.json", "error": "target field is unreadable"}
            ]
            return subprocess.CompletedProcess(command, result.returncode, json.dumps(value), "")
        return result

    with pytest.raises(driver.ObservationUnavailable, match="cannot be proved unrelated"):
        driver.run_tick(
            settings(tmp_path),
            fetch=authorities.fetch,
            run=unreadable,
            spawn=authorities.spawn,
        )
    assert authorities.launches == []


def test_tick_configuration_is_hourly_serial_and_bounded() -> None:
    config = yaml.safe_load((ROOT / "ci-hub" / "health" / "tick-hub.yaml").read_text())
    reminder = next(
        row for row in config["reminders"] if row["name"] == "integration_tip_validate"
    )
    assert reminder["cadence_secs"] == 3600
    assert reminder["gate"]["timeout_secs"] == 90
    assert driver.DEFAULT_TOTAL_TIMEOUT_SECONDS == 75
    assert reminder["gate"]["capture"] is True
    assert reminder["gate"]["when"] == "failure"
    assert "parallel" not in reminder["gate"]
    assert "integration_tip_validate.py" in reminder["gate"]["cmd"]

#!/usr/bin/env python3
"""Start one periodic validate when the exact Hermit main tip has no record or run."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import signal
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Mapping, Sequence


TOOL_ROOT = Path(__file__).resolve().parents[2]
STATE_ROOT = Path(os.environ.get("DEV_HERMIT_PARENT", TOOL_ROOT)).resolve()
sys.path.insert(0, str(TOOL_ROOT / "ci-hub" / "lib"))
import fetch_ref  # noqa: E402
import git_env  # noqa: E402


REPO = "rrnewton/hermit"
AGENT = "ops-tick-integration-tip"
DEFAULT_TOTAL_TIMEOUT_SECONDS = 75.0
EXACT_VALIDATE_STATUS_SCHEMA_VERSION = 3


class ObservationUnavailable(RuntimeError):
    """One required authority could not answer within this tick."""


@dataclass(frozen=True)
class Settings:
    tool_root: Path
    state_root: Path
    source_checkout: Path
    ci_hub: Path
    operational_tool: Path | None
    tool_parent_sha: str | None
    state_dir: Path
    total_timeout_seconds: float

    @classmethod
    def defaults(
        cls,
        *,
        tool_root: Path = TOOL_ROOT,
        state_root: Path = STATE_ROOT,
        source_checkout: Path | None = None,
        ci_hub: Path | None = None,
        operational_tool: Path | None = None,
        tool_parent_sha: str | None = None,
        state_dir: Path | None = None,
        total_timeout_seconds: float = DEFAULT_TOTAL_TIMEOUT_SECONDS,
    ) -> "Settings":
        state_root = state_root.resolve()
        configured_operational_tool = os.environ.get("DEV_HERMIT_OPERATIONAL_TOOL")
        configured_parent_sha = os.environ.get("DEV_HERMIT_TOOL_PARENT_SHA")
        return cls(
            tool_root=tool_root.resolve(),
            state_root=state_root,
            source_checkout=(source_checkout or state_root / "hermit").resolve(),
            ci_hub=(ci_hub or tool_root / "ci-hub" / "ci-hub").resolve(),
            operational_tool=(
                operational_tool
                or (Path(configured_operational_tool) if configured_operational_tool else None)
            ),
            tool_parent_sha=tool_parent_sha or configured_parent_sha,
            state_dir=(
                state_dir
                or state_root / "ignored" / "ci-hub" / "integration-tip-validate"
            ).resolve(),
            total_timeout_seconds=total_timeout_seconds,
        )


Runner = Callable[[Sequence[str], Path, float], subprocess.CompletedProcess[str]]
Fetcher = Callable[[Settings, float], str]
Spawner = Callable[[Settings, str, str, str, Path, Path, float], Mapping[str, object]]
LauncherActive = Callable[[Settings, Mapping[str, object], float, Runner], bool]


def one_line(value: object) -> str:
    return " ".join(str(value).split())


def emit(**fields: object) -> None:
    for key, value in fields.items():
        print(f"{key}={one_line(value)}")


def remaining(deadline: float, limit: float) -> float:
    value = min(limit, deadline - time.monotonic())
    if value <= 0:
        raise ObservationUnavailable("the decision deadline expired")
    return value


def run_bounded(
    command: Sequence[str], cwd: Path, timeout: float
) -> subprocess.CompletedProcess[str]:
    """Run one authority query and kill its whole process group on timeout."""

    try:
        process = subprocess.Popen(
            list(command),
            cwd=cwd,
            env=git_env.sanitized_git_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
    except OSError as error:
        raise ObservationUnavailable(f"cannot start {command[0]}: {error}") from error
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise ObservationUnavailable(
            f"{' '.join(command[:3])} timed out after {timeout:.1f}s"
        ) from error
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def fetch_tip(settings: Settings, timeout: float) -> str:
    try:
        result = fetch_ref.fetch_remote_tracking_branch(
            settings.source_checkout,
            remote="origin",
            branch="main",
            network_timeout_s=timeout,
        )
    except (OSError, fetch_ref.FetchRefError) as error:
        raise ObservationUnavailable(f"cannot fetch the exact Hermit main tip: {error}") from error
    # Another fetcher may already have reconciled the shared tracking ref to a
    # newer, remotely confirmed descendant. Every successful FetchResult makes
    # `tracking_sha` the authoritative tip. Selecting this process's stale
    # `fetched_sha` would consume the scheduled cadence without measuring the
    # current integration tip.
    return result.tracking_sha


def command_json(
    command: Sequence[str], *, cwd: Path, timeout: float, run: Runner
) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
    completed = run(command, cwd, timeout)
    try:
        value = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        detail = (
            completed.stderr.strip()
            or completed.stdout.strip()
            or f"exit {completed.returncode}"
        )
        raise ObservationUnavailable(
            f"{' '.join(command[:3])} returned unreadable JSON ({one_line(detail)}; {error})"
        ) from error
    if not isinstance(value, dict):
        raise ObservationUnavailable(f"{' '.join(command[:3])} did not return a JSON object")
    return completed, value


def exact_record_count(
    settings: Settings, target: str, timeout: float, run: Runner
) -> tuple[int, int, dict[str, object]]:
    completed, report = command_json(
        [
            str(settings.ci_hub),
            "validate-status",
            "--sha",
            target,
            "--repo",
            REPO,
            "--json",
        ],
        cwd=settings.state_root,
        timeout=timeout,
        run=run,
    )
    try:
        reported_sha = report["sha"]
        reported_exit = report["exit_code"]
        count = report["record_count"]
    except KeyError as error:
        raise ObservationUnavailable(
            f"exact validate-status omitted required fields: {error}"
        ) from error
    if (
        type(report.get("schema_version")) is not int
        or report.get("schema_version") != EXACT_VALIDATE_STATUS_SCHEMA_VERSION
        or report.get("repo") != REPO
        or reported_sha != target
        or type(reported_exit) is not int
        or reported_exit != completed.returncode
        or type(count) is not int
    ):
        raise ObservationUnavailable(
            "exact validate-status did not bind its JSON to the requested SHA and exit code"
        )
    if count < 0:
        raise ObservationUnavailable("exact validate-status returned a negative record count")
    unreadable = report.get("unreadable_handles")
    in_progress = report.get("in_progress")
    in_progress_count = report.get("in_progress_count")
    run_handles = report.get("run_handles")
    exact_records = report.get("exact_records")
    if (
        not isinstance(unreadable, list)
        or not isinstance(in_progress, list)
        or not isinstance(run_handles, list)
        or not isinstance(exact_records, list)
    ):
        raise ObservationUnavailable(
            "exact validate-status omitted exact_records, run_handles, in_progress, "
            "or unreadable_handles"
        )
    if unreadable:
        raise ObservationUnavailable(
            f"exact validate-status could not read {len(unreadable)} run handle(s); "
            "a malformed handle without a readable target cannot be proved unrelated"
        )
    if type(in_progress_count) is not int or in_progress_count != len(in_progress):
        raise ObservationUnavailable("exact validate-status returned inconsistent in-progress data")
    if any(
        not isinstance(row, dict)
        or row.get("sha") != target
        or row.get("in_progress") is not True
        for row in in_progress
    ):
        raise ObservationUnavailable("exact validate-status returned an unrelated in-progress row")
    if any(
        not isinstance(handle, dict) or handle.get("sha") != target
        for handle in run_handles
    ):
        raise ObservationUnavailable("exact validate-status returned an unrelated run handle")
    if count < len(exact_records) or (count == 0 and exact_records):
        raise ObservationUnavailable(
            "exact validate-status returned inconsistent exact-record data"
        )
    if any(
        not isinstance(record, dict)
        or record.get("verdict")
        not in {
            "VALIDATED",
            "FAILED",
            "TRUNCATED",
            "NEEDS-RERUN",
            "NO-RESULT",
            "NOT-VALIDATED",
        }
        or not isinstance(record.get("detail"), str)
        or not str(record["detail"]).strip()
        for record in exact_records
    ):
        raise ObservationUnavailable("exact validate-status returned a malformed exact record")
    if count == 0:
        freshness = report.get("ledger_freshness")
        if not isinstance(freshness, dict) or freshness.get(
            "local_absence_is_authoritative"
        ) is not True:
            raise ObservationUnavailable(
                "the validation ledger could not establish that this exact tip has no record"
            )
    return count, in_progress_count, report


def admission_status(
    settings: Settings, target: str, timeout: float, run: Runner
) -> dict[str, object]:
    completed, report = command_json(
        [
            str(settings.ci_hub),
            "validate-lock",
            "admission-status",
            "--agent",
            AGENT,
            "--kind",
            "validate",
            "--target",
            target,
            "--target-was-fetched-main",
            "--json",
        ],
        cwd=settings.state_root,
        timeout=timeout,
        run=run,
    )
    admissible = report.get("admissible")
    if (
        report.get("schema_version") != 1
        or report.get("kind") != "validate"
        or not isinstance(admissible, bool)
        or report.get("target") != target
    ):
        raise ObservationUnavailable("validate-lock admission-status returned inconsistent JSON")
    state = report.get("state")
    expected = {
        "admissible": 0,
        "busy": 1,
        "unknown": 2,
        "refused": 3,
        "quarantined": 3,
        "stale": 3,
    }.get(state)
    if expected is None:
        raise ObservationUnavailable(
            f"validate-lock admission-status returned unsupported state {state!r}"
        )
    if completed.returncode != expected:
        raise ObservationUnavailable(
            f"validate-lock admission-status JSON disagrees with exit {completed.returncode}"
        )
    if admissible != (state == "admissible"):
        raise ObservationUnavailable(
            "validate-lock admission-status state disagrees with admissible"
        )
    return report


def atomic_write_json(path: Path, value: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}-{uuid.uuid4().hex}")
    try:
        with temporary.open("x", encoding="utf-8") as stream:
            json.dump(dict(value), stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def read_json(path: Path) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    except (OSError, json.JSONDecodeError) as error:
        raise ObservationUnavailable(f"cannot read prior tick attempt {path}: {error}") from error
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ObservationUnavailable(f"prior tick attempt {path} has an unsupported shape")
    return value


def launcher_unit_is_active(
    settings: Settings,
    attempt: Mapping[str, object],
    timeout: float,
    run: Runner,
) -> bool:
    unit = attempt.get("launcher_unit")
    if not isinstance(unit, str) or not unit.endswith(".service"):
        return False
    completed = run(
        [
            "systemctl",
            "--user",
            "show",
            unit,
            "--property=LoadState",
            "--property=ActiveState",
            "--property=SubState",
            "--property=InvocationID",
            "--no-pager",
        ],
        settings.state_root,
        timeout,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"exit {completed.returncode}"
        raise ObservationUnavailable(
            f"cannot query periodic validate launcher unit {unit}: {one_line(detail)}"
        )
    fields = dict(
        line.split("=", 1) for line in completed.stdout.splitlines() if "=" in line
    )
    if fields.get("LoadState") == "not-found":
        return False
    if fields.get("LoadState") != "loaded" or not fields.get("InvocationID"):
        raise ObservationUnavailable(
            f"periodic validate launcher unit {unit} returned incomplete systemd identity"
        )
    return fields.get("ActiveState") in {
        "activating",
        "active",
        "reloading",
        "deactivating",
    }


def tick_attempt_is_active(
    settings: Settings,
    attempt: Mapping[str, object] | None,
    timeout: float,
    run: Runner,
    active: LauncherActive,
) -> bool:
    return attempt is not None and active(settings, attempt, timeout, run)


def latest_prior_attempt(
    tick_attempt: Mapping[str, object] | None,
    run_handles: object,
) -> Mapping[str, object] | None:
    candidates: list[Mapping[str, object]] = []
    if tick_attempt is not None:
        candidates.append(tick_attempt)
    if isinstance(run_handles, list):
        candidates.extend(handle for handle in run_handles if isinstance(handle, dict))
    return max(
        candidates,
        key=lambda row: str(row.get("started_at", "")),
        default=None,
    )


def spawn_validate(
    settings: Settings,
    target: str,
    attempt_id: str,
    unit: str,
    validate_log: Path,
    launcher_log: Path,
    timeout: float,
) -> Mapping[str, object]:
    operational_tool = settings.operational_tool
    parent_sha = settings.tool_parent_sha
    if (
        operational_tool is None
        or not operational_tool.is_file()
        or not os.access(operational_tool, os.X_OK)
    ):
        raise ObservationUnavailable(
            "the verified installed operational-tool is unavailable; refusing a launcher "
            "that would depend on the health tick's short-lived tool snapshot"
        )
    if (
        parent_sha is None
        or len(parent_sha) != 40
        or any(character not in "0123456789abcdef" for character in parent_sha)
    ):
        raise ObservationUnavailable(
            "the exact parent tool SHA is unavailable; refusing an unbound launcher"
        )
    launcher_log.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(launcher_log, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    os.close(descriptor)
    launcher_unit = f"{unit}-launcher"
    validate_arguments = [
        "validate-run",
        "--checkout",
        str(settings.source_checkout),
        "--state-root",
        str(settings.state_root),
        "--repo",
        REPO,
        "--materialize-target",
        "--agent",
        AGENT,
        "--target",
        target,
        "--unit",
        unit,
        "--log",
        str(validate_log),
        "--no-wait",
        "--skip-if-recorded",
        "--json",
        "--",
        "full",
    ]
    command = [
        "systemd-run",
        "--user",
        "--quiet",
        "--collect",
        f"--unit={launcher_unit}",
        "--description=start one periodic exact-tip Hermit validate",
        "--property=RuntimeMaxSec=5000",
        f"--property=StandardOutput=append:{launcher_log}",
        f"--property=StandardError=append:{launcher_log}",
        f"--working-directory={settings.state_root}",
        f"--setenv=PATH={os.environ.get('PATH', '')}",
        f"--setenv=DEV_HERMIT_PARENT={settings.state_root}",
        "--",
        str(operational_tool),
        "--run-current-tool",
        "--state-root",
        str(settings.state_root),
        "--tool-parent-sha",
        parent_sha,
        "--tool-relative",
        "ci-hub/ci-hub",
        "--",
        *validate_arguments,
    ]
    completed = run_bounded(command, settings.state_root, timeout)
    if completed.returncode != 0:
        detail = (
            completed.stderr.strip()
            or completed.stdout.strip()
            or f"exit {completed.returncode}"
        )
        raise ObservationUnavailable(
            f"systemd-run refused {launcher_unit}: {one_line(detail)}"
        )
    return {"unit": f"{launcher_unit}.service", "accepted": True}


def prior_attempt_text(prior: Mapping[str, object] | None) -> str:
    if prior is None:
        return (
            "no readable prior attempt record was found; ledger absence alone cannot tell "
            "whether validation was never attempted or ended before writing a ledger row"
        )
    name = prior.get("attempt_id") or prior.get("unit") or "?"
    state = prior.get("state") or "unknown"
    result = prior.get("result") or "none"
    exit_code = prior.get("exit_code")
    exit_text = "unknown" if exit_code is None else str(exit_code)
    detail = prior.get("detail")
    detail_text = f" detail={one_line(detail)}" if detail else ""
    return (
        f"prior attempt {name} ended or stopped being observable "
        f"with state={state} result={result} exit={exit_text}{detail_text} "
        "and no ledger row"
    )


def record_status_text(report: Mapping[str, object]) -> str:
    records = report["exact_records"]
    if not records:
        return (
            "the raw record identity is present, but its current row was corrected "
            "or superseded"
        )
    pieces: list[str] = []
    for record in records:
        exit_code = record.get("exit_code")
        exit_text = "unknown" if exit_code is None else str(exit_code)
        result = record.get("result") or "none"
        pieces.append(
            f"{record['verdict']} result={result} exit={exit_text} "
            f"detail={one_line(record['detail'])}"
        )
    return "; ".join(pieces)


def run_tick(
    settings: Settings,
    *,
    fetch: Fetcher = fetch_tip,
    run: Runner = run_bounded,
    spawn: Spawner = spawn_validate,
    launcher_active: LauncherActive = launcher_unit_is_active,
    dry_run: bool = False,
) -> int:
    deadline = time.monotonic() + settings.total_timeout_seconds
    target = fetch(settings, remaining(deadline, 25.0))
    count, in_progress, status = exact_record_count(
        settings, target, remaining(deadline, 35.0), run
    )
    if count:
        verdict = str(status["verdict"])
        detail = record_status_text(status)
        next_action = (
            "none; this exact tip is already validated"
            if verdict == "VALIDATED"
            else (
                "inspect the recorded outcome and explicitly rerun this exact tip "
                "when appropriate; no automatic duplicate was started"
            )
        )
        emit(
            state=("record-present" if verdict == "VALIDATED" else verdict.lower()),
            target=target,
            record_count=count,
            verdict=verdict,
            record_detail=detail,
            launched=0,
            next_action=next_action,
            summary=(
                f"exact Hermit main tip {target} has verdict {verdict}: {detail}; "
                f"{next_action}"
            ),
        )
        return int(status["exit_code"])

    # The canonical run-handle authority already proved these exact-target
    # processes live from their cgroup-bound identities.  Honor that answer
    # before consulting our narrower launcher receipt: a failed systemd query
    # must not turn a known in-progress validation into UNKNOWN.
    if in_progress:
        emit(
            state="in-progress",
            target=target,
            record_count=0,
            in_progress_count=in_progress,
            launched=0,
            summary=(
                f"exact Hermit main tip {target} already has {in_progress} "
                "validate run(s) in progress; no validate started"
            ),
        )
        return 0

    tick_attempt = read_json(settings.state_dir / "latest.json")
    if tick_attempt is not None and tick_attempt.get("target") != target:
        tick_attempt = None
    if tick_attempt_is_active(
        settings,
        tick_attempt,
        remaining(deadline, 5.0),
        run,
        launcher_active,
    ):
        assert tick_attempt is not None
        emit(
            state="in-progress",
            target=target,
            record_count=0,
            in_progress_count=1,
            launched=0,
            summary=(
                f"launch attempt {tick_attempt.get('attempt_id', '?')} for exact Hermit main "
                "tip is still running; no second validate started"
            ),
        )
        return 0

    prior = latest_prior_attempt(tick_attempt, status["run_handles"])
    admission = admission_status(settings, target, remaining(deadline, 10.0), run)
    if admission["admissible"] is not True:
        reason = one_line(admission.get("detail", admission.get("reason_code", "unknown")))
        admission_state = str(admission["state"])
        emit(
            state=admission_state,
            target=target,
            record_count=0,
            in_progress_count=0,
            launched=0,
            prior_attempt=prior_attempt_text(prior),
            next_action="retry on the next scheduled tick when immediate admission is available",
            summary=(
                "validate-lock cannot admit an immediate validate for exact "
                f"Hermit main tip {target}: {reason}; no validate started"
            ),
        )
        return 0 if admission_state == "busy" else 2

    if dry_run:
        prior_text = prior_attempt_text(prior)
        emit(
            state="would-launch",
            target=target,
            record_count=0,
            in_progress_count=0,
            launched=0,
            prior_attempt=prior_text,
            next_action="run without --dry-run to start one validate",
            summary=(
                f"would start one asynchronous validate for exact Hermit main tip {target}; "
                f"{prior_text}; run without --dry-run to start it"
            ),
        )
        return 0

    observed_at = datetime.now(timezone.utc).isoformat()
    token = uuid.uuid4().hex[:12]
    attempt_id = f"{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}-{token}"
    unit = f"validate-ops-tick-{target[:12]}-{token}"
    logs = settings.state_dir / "logs"
    validate_log = logs / f"{attempt_id}-validate.log"
    launcher_log = logs / f"{attempt_id}-launcher.log"
    attempt_path = settings.state_dir / "latest.json"
    attempt: dict[str, object] = {
        "schema_version": 1,
        "attempt_id": attempt_id,
        "state": "starting",
        "target": target,
        "repo": REPO,
        "agent": AGENT,
        "unit": f"{unit}.service",
        "launcher_unit": f"{unit}-launcher.service",
        "started_at": observed_at,
        "validate_log": str(validate_log),
        "launcher_log": str(launcher_log),
    }
    atomic_write_json(attempt_path, attempt)
    try:
        launch = spawn(
            settings,
            target,
            attempt_id,
            unit,
            validate_log,
            launcher_log,
            remaining(deadline, 10.0),
        )
    except Exception as error:
        attempt.update(
            state="launch-failed",
            detail=one_line(error),
            finished_at=datetime.now(timezone.utc).isoformat(),
        )
        atomic_write_json(attempt_path, attempt)
        emit(
            state="launch-failed",
            target=target,
            record_count=0,
            in_progress_count=0,
            launched=0,
            prior_attempt=prior_attempt_text(prior),
            next_action="repair the launch refusal, then retry this exact tip",
            summary=(
                "validate launch failed before a ledger row was written for exact "
                f"Hermit main tip {target}: {one_line(error)}"
            ),
        )
        return 1
    attempt.update(state="launcher-accepted", launcher=dict(launch))
    atomic_write_json(attempt_path, attempt)
    emit(
        state="launched",
        target=target,
        record_count=0,
        in_progress_count=0,
        launched=1,
        attempt=attempt_id,
        prior_attempt=prior_attempt_text(prior),
        next_action=f"follow {validate_log}; read validate-status when the run finishes",
        summary=(
            f"started one asynchronous validate for exact Hermit main tip {target}; "
            f"{prior_attempt_text(prior)}; follow {validate_log} and read validate-status "
            "when the run finishes"
        ),
    )
    return 0


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    result.add_argument("--state-root", type=Path, default=STATE_ROOT)
    result.add_argument("--tool-root", type=Path, default=TOOL_ROOT)
    result.add_argument("--source-checkout", type=Path)
    result.add_argument("--ci-hub", type=Path)
    result.add_argument("--state-dir", type=Path)
    result.add_argument(
        "--dry-run",
        action="store_true",
        help="report whether a validate would start without creating a run or launcher",
    )
    result.add_argument(
        "--total-timeout-seconds",
        type=float,
        default=DEFAULT_TOTAL_TIMEOUT_SECONDS,
    )
    return result


def main(argv: Sequence[str] | None = None) -> int:
    args = parser().parse_args(argv)
    if args.total_timeout_seconds <= 0 or args.total_timeout_seconds >= 180:
        raise SystemExit("--total-timeout-seconds must be greater than 0 and less than 180")
    settings = Settings.defaults(
        tool_root=args.tool_root,
        state_root=args.state_root,
        source_checkout=args.source_checkout,
        ci_hub=args.ci_hub,
        state_dir=args.state_dir,
        total_timeout_seconds=args.total_timeout_seconds,
    )
    settings.state_dir.mkdir(parents=True, exist_ok=True)
    lock_path = settings.state_dir / "tick.lock"
    try:
        with lock_path.open("a+") as lock:
            try:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                emit(
                    state="in-progress",
                    launched=0,
                    summary=(
                        "another operational tick is already deciding whether to "
                        "start this validate; no second decision started"
                    ),
                )
                return 0
            try:
                return run_tick(settings, dry_run=args.dry_run)
            except ObservationUnavailable as error:
                emit(
                    state="unknown",
                    launched=0,
                    summary=(
                        "could not decide whether to start the periodic validate: "
                        f"{one_line(error)}"
                    ),
                )
                return 2
    except OSError as error:
        emit(
            state="unknown",
            launched=0,
            summary=f"could not lock periodic validate state at {lock_path}: {one_line(error)}",
        )
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

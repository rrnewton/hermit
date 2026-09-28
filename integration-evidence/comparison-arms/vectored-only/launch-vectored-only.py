#!/usr/bin/env python3
"""Launch the reviewed vectored-only comparison diagnostic through validate-lock."""

from __future__ import annotations

import argparse
import datetime as dt
import os
import shutil
import subprocess
import sys
from pathlib import Path


STATE_ROOT = Path("/home/newton/work/dev-hermit")
TOOL_ROOT = STATE_ROOT
CHECKOUT = STATE_ROOT / "worktrees/slots/integration-pr529-pr538-diagnostic"
TARGET = "5c0bae832d515ce2b8b9cfcf2d7b97f010d641c6"
AGENT = "kvm-ratchet"
CHILD_DEADLINE_SECONDS = 5400
WAIT_SECONDS = 600
HOLD_SECONDS = 600
UNIT_RUNTIME_SECONDS = WAIT_SECONDS + CHILD_DEADLINE_SECONDS + 300

sys.path.insert(0, str(TOOL_ROOT / "ci-hub/validate"))
import run_registry  # noqa: E402


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def update_terminal(record: Path, returncode: int, measurement: Path) -> None:
    passed = returncode == 0
    run_registry.update_record(
        record,
        state="completed" if passed else "failed",
        result="passed" if passed else "failed",
        exit_code=returncode,
        detail=None if passed else f"bounded 27-cell launcher exited {returncode}",
        finished_at=utc_now(),
        results=str(measurement),
    )


def worker(unit: str, measurement: Path) -> int:
    record = run_registry.record_path(STATE_ROOT, unit)
    run_registry.update_record(record, state="running")
    command = [
        str(TOOL_ROOT / "ci-hub/ci-hub"),
        "validate-lock",
        "run",
        "--agent",
        AGENT,
        "--kind",
        "bench",
        "--target",
        TARGET,
        "--run-record",
        str(record),
        "--wait",
        str(WAIT_SECONDS),
        "--hold",
        str(HOLD_SECONDS),
        "--child-deadline",
        str(CHILD_DEADLINE_SECONDS),
        "--max",
        "1",
        "--",
        "/usr/bin/bash",
        str(CHECKOUT / "integration-evidence/comparison-arms/vectored-only/measure-vectored-only-cells.sh"),
    ]
    environment = dict(os.environ)
    environment["DEV_HERMIT_PARENT"] = str(STATE_ROOT)
    environment["KVM_MEASUREMENT_DIR"] = str(measurement)
    environment["KVM_MEASURE_CHILD_DEADLINE_SECONDS"] = str(CHILD_DEADLINE_SECONDS)
    try:
        completed = subprocess.run(command, cwd=CHECKOUT, env=environment, check=False)
        update_terminal(record, completed.returncode, measurement)
        return completed.returncode
    except Exception as error:
        run_registry.update_record(
            record,
            state="failed",
            result="failed",
            exit_code=1,
            detail=f"worker exception: {error}",
            finished_at=utc_now(),
            results=str(measurement),
        )
        raise


def launch(unit: str, measurement: Path) -> int:
    if not unit.startswith("validate-kvm-vectored-only-"):
        raise RuntimeError(f"unexpected unit name: {unit}")
    if measurement.parent != CHECKOUT / "integration-evidence":
        raise RuntimeError(f"measurement is outside integration evidence: {measurement}")
    if not measurement.name.startswith("brs-vectored-only-"):
        raise RuntimeError(f"measurement is not the expected bounded subvolume: {measurement}")

    systemd_run = shutil.which("systemd-run")
    if systemd_run is None:
        raise RuntimeError("systemd-run is unavailable")
    record = run_registry.record_path(STATE_ROOT, unit)
    log = STATE_ROOT / "ignored/validate" / f"{unit}.log"
    run_registry.create_current_record(
        record,
        {
            "schema_version": run_registry.SCHEMA_VERSION,
            "kind": "bench",
            "state": "launching",
            "unit": f"{unit}.service",
            "target": TARGET,
            "repo": "rrnewton/hermit",
            "checkout": str(CHECKOUT),
            "log": str(log),
            "agent": AGENT,
            "started_at": utc_now(),
            "producer": run_registry.PRODUCER,
            "admission": "ci-hub validate-lock",
        },
    )
    run_registry.reserve_log(log)
    command = [
        systemd_run,
        "--user",
        "--quiet",
        "--collect",
        f"--unit={unit}",
        "--description=bounded vectored-only 27-cell KVM comparison",
        f"--property=RuntimeMaxSec={UNIT_RUNTIME_SECONDS}",
        "--property=MemoryMax=32G",
        "--property=MemorySwapMax=0",
        f"--property=StandardOutput=append:{log}",
        f"--property=StandardError=append:{log}",
        f"--working-directory={CHECKOUT}",
        f"--setenv=PATH={os.environ.get('PATH', '')}",
        f"--setenv=DEV_HERMIT_PARENT={STATE_ROOT}",
        f"--setenv=KVM_MEASUREMENT_DIR={measurement}",
        f"--setenv=KVM_MEASURE_CHILD_DEADLINE_SECONDS={CHILD_DEADLINE_SECONDS}",
        "--",
        sys.executable,
        str(Path(__file__).resolve()),
        "_worker",
        "--unit",
        unit,
        "--measurement",
        str(measurement),
    ]
    completed = subprocess.run(command, cwd=TOOL_ROOT, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip() or "no output"
        run_registry.update_record(
            record,
            state="refused",
            result="systemd-launch-refused",
            exit_code=completed.returncode,
            detail=detail,
            finished_at=utc_now(),
        )
        raise RuntimeError(f"systemd-run refused service: {detail}")
    print(f"unit={unit}.service")
    print(f"record={record}")
    print(f"log={log}")
    print(f"measurement={measurement}")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("launch", "_worker"))
    parser.add_argument("--unit", required=True)
    parser.add_argument("--measurement", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    measurement = args.measurement.resolve()
    if args.mode == "_worker":
        return worker(args.unit, measurement)
    return launch(args.unit, measurement)


if __name__ == "__main__":
    raise SystemExit(main())

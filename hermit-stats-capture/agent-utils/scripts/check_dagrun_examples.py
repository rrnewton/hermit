#!/usr/bin/env python3
"""Validate every shipped DAG example under one selected implementation."""

from __future__ import annotations

import argparse
import csv
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
VALIDATION_JOBS_ENV = "AGENT_UTILS_VALIDATION_JOBS"
DEFAULT_VALIDATION_JOBS = 8
FIXED_WIDTH_EXAMPLE = "05-inner-jobs.json"
FIXED_WIDTH_JOBS = 8


def _effective_validation_jobs() -> int:
    """Return this process's effective whole-core affinity/quota budget."""

    python_root = str(REPO_ROOT / "py")
    inserted = python_root not in sys.path
    if inserted:
        sys.path.insert(0, python_root)
    try:
        from dagrun.profile_enrich import container_core_budget

        return container_core_budget()
    finally:
        if inserted:
            sys.path.remove(python_root)


def _validation_jobs(
    environment: dict[str, str] | None = None, *, effective_jobs: int | None = None
) -> int:
    """Read the nested CPU budget delivered through the outer DAG's jobs_env channel."""

    environ = os.environ if environment is None else environment
    effective = _effective_validation_jobs() if effective_jobs is None else effective_jobs
    if effective < 1:
        raise ValueError(f"effective CPU budget must be positive, got {effective}")
    raw = environ.get(VALIDATION_JOBS_ENV)
    if raw is None:
        return min(effective, DEFAULT_VALIDATION_JOBS)
    if not raw or not raw.isascii() or not raw.isdecimal():
        raise ValueError(
            f"{VALIDATION_JOBS_ENV} must be a positive integer, got {raw!r}"
        )
    jobs = int(raw)
    if jobs < 1:
        raise ValueError(
            f"{VALIDATION_JOBS_ENV} must be a positive integer, got {raw!r}"
        )
    return min(jobs, effective, DEFAULT_VALIDATION_JOBS)


def _examples() -> list[Path]:
    paths = sorted((REPO_ROOT / "examples").glob("*.json"))
    paths.extend(sorted((REPO_ROOT / "examples").glob("*.yaml")))
    if not paths:
        raise RuntimeError("no examples/*.json or examples/*.yaml files were found")
    return paths


def _engine(name: str) -> tuple[list[str], dict[str, str]]:
    environment = os.environ.copy()
    # This is a harness control, not input to the nested dagrun or its example guests.
    environment.pop(VALIDATION_JOBS_ENV, None)
    if name == "python":
        python_path = str(REPO_ROOT / "py")
        inherited = environment.get("PYTHONPATH")
        environment["PYTHONPATH"] = (
            os.pathsep.join((python_path, inherited)) if inherited else python_path
        )
        return [sys.executable, "-m", "dagrun"], environment
    return [str(REPO_ROOT / "rs/bin/dagrun")], environment


def _profile_smoke(
    command: list[str], environment: dict[str, str], *, validation_jobs: int
) -> None:
    """Exercise the real profile table and default store for one engine."""

    with tempfile.TemporaryDirectory(prefix="dagrun-example-profile-") as raw:
        root = Path(raw)
        dag = root / "profile-smoke.json"
        dag.write_text(
            json.dumps(
                {
                    "mem_cap_factor": 1.0,
                    "mem_cap_floor_bytes": 0,
                    "steps": [
                        {
                            "group": "profile",
                            "job": "smoke",
                            "cmd": "true",
                            "hint": {"rss_baseline_bytes": 1048576},
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        profile_environment = environment.copy()
        profile_environment.pop("DAGRUN_PROFILE_DIR", None)
        completed = subprocess.run(
            [
                *command,
                "run",
                "--dag",
                str(dag),
                "--max-cpus",
                str(validation_jobs),
                "--allow-cgroup-failure",
                "--profile",
                "--no-profile-feedback",
            ],
            cwd=root,
            env=profile_environment,
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode != 0:
            raise RuntimeError(
                f"profile smoke failed with exit {completed.returncode}:\n"
                f"{completed.stdout}\n{completed.stderr}"
            )
        if "per-step profile:" not in completed.stdout or "profile.smoke" not in completed.stdout:
            raise RuntimeError("--profile output omitted its table or real step row")
        stores = sorted((root / ".dagrun" / "profiles").glob("step_profiles_*.csv"))
        if len(stores) != 1:
            raise RuntimeError(f"default profile store has {len(stores)} step CSVs, expected one")
        with stores[0].open(newline="", encoding="utf-8") as handle:
            rows = list(csv.DictReader(handle))
        if len(rows) != 1 or rows[0].get("step") != "profile.smoke":
            raise RuntimeError("default profile store omitted the successful profile.smoke row")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True, choices=("python", "rust"))
    arguments = parser.parse_args(argv)
    try:
        validation_jobs = _validation_jobs()
    except ValueError as error:
        parser.error(str(error))
    command, environment = _engine(arguments.engine)
    examples = _examples()
    for path in examples:
        relative = path.relative_to(REPO_ROOT)
        print(f"examples[{arguments.engine}]: {relative}", flush=True)
        expected_refusal = (
            path.name == FIXED_WIDTH_EXAMPLE and validation_jobs < FIXED_WIDTH_JOBS
        )
        completed = subprocess.run(
            [
                *command,
                "run",
                "--dag",
                str(path),
                "--allow-cgroup-failure",
                "--no-profile",
                "--max-cpus",
                str(validation_jobs),
            ],
            cwd=REPO_ROOT,
            env=environment,
            text=expected_refusal,
            capture_output=expected_refusal,
            check=False,
        )
        if expected_refusal:
            combined = completed.stdout + completed.stderr
            if (
                completed.returncode == 2
                and "cannot lower guest parallelism" in combined
                and "build.app (preferred_inner_jobs=8)" in combined
                and f"--max-cpus {validation_jobs}" in combined
                and "[build] make -j8 build" not in combined
            ):
                print(
                    f"examples[{arguments.engine}]: {relative} correctly refused at "
                    f"{validation_jobs} CPU(s) (fixed width {FIXED_WIDTH_JOBS})",
                    flush=True,
                )
                continue
            print(completed.stdout, end="", file=sys.stdout)
            print(completed.stderr, end="", file=sys.stderr)
            print(
                f"examples[{arguments.engine}]: expected documented fixed-width refusal at "
                f"{validation_jobs} CPU(s)",
                file=sys.stderr,
            )
            return 1
        if completed.returncode != 0:
            return completed.returncode
    try:
        _profile_smoke(command, environment, validation_jobs=validation_jobs)
    except RuntimeError as error:
        print(f"examples[{arguments.engine}]: {error}", file=sys.stderr)
        return 1
    print(f"examples[{arguments.engine}]: {len(examples)} validated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

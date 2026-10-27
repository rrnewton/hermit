#!/usr/bin/env python3
"""Launch the nightly pressure-test sweep of every red cell without overlapping validate."""

from __future__ import annotations

import argparse
import datetime as dt
import fcntl
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import time
import zoneinfo
from pathlib import Path
from typing import Callable, Mapping, Sequence


SOURCE_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "series"))
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "validate"))
sys.path.insert(0, str(SOURCE_ROOT / "ci-hub" / "lib"))
import measurement_hold  # noqa: E402
import series  # noqa: E402
import run_registry  # noqa: E402
import cpu_placement  # noqa: E402
import unwrapped_git  # noqa: E402


STATE_ROOT = Path(
    os.environ.get("DEV_HERMIT_PARENT", series.shared_parent_root(SOURCE_ROOT))
).resolve()
UNIT = "hermit-pressure-test"
# THE NIGHTLY SWEEP (owner, 2026-10-18: "Even if we're not doing product work we
# should have a nightly pressure test so as to ensure that all green cells are
# selected for all backends"). Once a night, at or after 01:00 US Eastern, one
# run takes every red cell of the scorecard (pressure-test.rs's default
# population, no --sample) through SWEEP_REPETITIONS independent first-attempt
# repetitions (--no-retry), under each cell's manifest budget and comparators.
# It replaces the daily ten-cell sample, a subset of the same population. The
# health tick calls launch every 30 minutes; outside the window, or once
# tonight's sweep has started, it launches nothing (sweep_due).
SWEEP_ZONE = zoneinfo.ZoneInfo("America/New_York")
SWEEP_WINDOW_START_HOUR = 1
SWEEP_WINDOW_END_HOUR = 6
SWEEP_REPETITIONS = 3
SWEEP_CLOCK_ENV = "HERMIT_PRESSURE_SWEEP_CLOCK"
# Width: manifest guests at once, half the host's idle cores measured from
# /proc/stat once the bench admission is held, so the sweep keeps at least half
# of the idle CPU free; then no more than pressure-test.rs's highest
# memory-safe manifest-guest cap for the part's own population, read from a
# `plan` of the same selection (memory_safe_guests). The load average is no
# measure of idle CPU on this host: on devbig014 it stayed between 250 and 300
# around the clock on 2026-10-17/18 while /proc/stat read 91.5% of 316 cores
# idle. Until 2026-10-25 the width was also capped at 32 guests, and the night
# of 2026-10-25 ran every part at 32 with 251-290 cores idle (owner, 4:15 PM
# ET: "size it from measured headroom instead").
SWEEP_IDLE_SHARE = 0.5
SWEEP_IDLE_SAMPLE_SECONDS = 10
# No per-cell bound is passed: pressure-test.rs derives each cell's enclosing
# allowance from its manifest budget (at least 278 s for a 57 s manifest
# timeout; https://github.com/rrnewton/dev-hermit/issues/431) and refuses a
# plan whose declared worst-case occupancy, the allowances summed over every
# repetition and divided by the guest width, cannot fit --run-timeout. Planned
# at hermit 886474838 on 2026-10-18: 506 red cells x 3 repetitions = 1518 runs
# declaring 463,788 s, 14,493 s at 32 guests (measured cells take a median
# 2.2 s). 21,600 s admits that population at 22 guests or more, so the sweep
# refuses, by name, a width below SWEEP_MIN_GUESTS rather than launching a run
# that pressure-test.rs would refuse. pressure-test.rs (audit_dag) also refuses
# any copied build step whose wall timeout is not below the run timeout; the
# largest, build.buck_release_artifact, has 1800 s.
SWEEP_MIN_GUESTS = 24
RUN_TIMEOUT_SECONDS = 21600
# THE NIGHT'S PARTS (coordinator ruling on the owner's directive, 2026-10-18
# 8:11 AM ET). Each part is its own pressure-test run under its own bench
# admission, so a waiting validate gets the box between parts, and a part that
# fails or hangs costs only its own bound. In order:
#   red          every red cell, SWEEP_REPETITIONS times (above);
#   sample       the former daily sample of SAMPLE_COUNT red cells, kept for
#                its parity post-pass: pressure-test.rs runs ptrace parity
#                references only for a --sample run, about 7 scored pairs a
#                day, which the red sweep alone would silently stop;
#   survey-<b>   the cells of backend <b> that the scorecard marks
#                not-applicable because they failed a recorded survey
#                (SURVEY_FAILED_RE), and those a weekly re-survey promoted,
#                through --probe-disabled --backend <b> --cells-file,
#                SWEEP_REPETITIONS times: "Survey failures are exactly where
#                green cells hide after fixes land." Owner-held cells
#                (OWNER_HELD_RE) stay the owner's call and are excluded. A
#                backend with no such cell records `empty` and runs nothing.
#   resurvey-<b> once a week, on the night that starts Sunday 01:00 US
#                Eastern, every executable disabled cell of backend <b>
#                (--probe-disabled --backend <b>, no cells file) once: "to
#                catch backends that gained a capability". A pass there flips
#                nothing; a CLEAN cell that is not owner-held joins the
#                nightly survey population (PROMOTED, sweep-promoted.jsonl).
# Planned at hermit 886474838: survey-kvm 33 runs (its floor is the 4-guest
# KVM cap, 2,294 s), survey-liteinst 462 and survey-in-guest-trap 468 runs
# (5,352 and 5,421 s at 24 guests), so a survey part's bound is 7,200 s, or
# its population's declared need at SWEEP_MIN_GUESTS when larger, up to
# SURVEY_MAX_RUN_TIMEOUT_SECONDS. The weekly populations were 345 (ptrace) to
# 940 (dbt) cells, declaring up to 261,320 s: 10,888 s at 24 guests, inside
# RESURVEY_RUN_TIMEOUT_SECONDS.
SAMPLE_COUNT = 10
SAMPLE_RUN_TIMEOUT_SECONDS = 2700
ALL_BACKENDS = ("ptrace", "sabre", "native", "kvm", "dbt", "liteinst", "in-guest-trap")
SURVEY_RUN_TIMEOUT_SECONDS = 7200
SURVEY_MAX_RUN_TIMEOUT_SECONDS = 14400
# Each probed cell declares at most this enclosing allowance (278-290 s for the
# survey populations planned on 2026-10-18).
SURVEY_CELL_ALLOWANCE_SECONDS = 300
RESURVEY_RUN_TIMEOUT_SECONDS = 14400
RESURVEY_WEEKDAY = 6  # Sunday, as datetime.date.weekday() counts
SURVEY_FAILED_RE = re.compile(r"survey|Did not pass \d+ of \d+", re.IGNORECASE)
OWNER_HELD_RE = re.compile(
    r"Owner reset|Owner directive|coordinator ruling", re.IGNORECASE
)
# A night's part that has not started by 07:00 US Eastern is skipped and
# recorded, so the sweep does not run into the working day. A campaign launched
# by hand outside the window (launch --now) runs every part.
SWEEP_LAST_PART_START_HOUR = 7
# THE SAMPLE BUILDS ITS RUST SCRIPTS IN ITS OWN RUST-SCRIPT CACHE.
# pressure-test.rs and the scripts it runs are rust-scripts, and every
# rust-script on the host otherwise builds into the one target directory under
# ~/.cache/rust-script, which Cargo locks for each build; lanes build into it
# every few minutes. Measured on devbig014, 2026-10-08: a build with nothing to
# do took 0.13 s alone and 133.5 s behind one other lane's scorecard build. The
# cache persists between daily samples so dependencies stay built, and only the
# validate lock's holder (run_locked) uses it, so nothing else can be using it
# when it is measured or removed. The checkout path is new for every sample, so
# each sample adds its own package and binaries (about 8 MB per pressure-test
# binary when measured); the cache is removed whole before a sample once it
# exceeds the bound, rather than being added to without limit.
PRESSURE_RUST_SCRIPT_CACHE = "pressure-test-rust-script-cache"
PRESSURE_RUST_SCRIPT_CACHE_MAX_BYTES = 8 * 1024**3
LOCK_WAIT_SECONDS = 3600
# The validate-lock child is `_locked`: the idle sample, the run inside a scope
# that adds max(60, run/10) s of grace (23,760 s in all for the red part), then
# emit-series, which took 3-6 minutes with its ledger scans in October 2026.
EMIT_ALLOWANCE_SECONDS = 1200


def child_deadline(run_timeout: int) -> int:
    return run_timeout + max(60, run_timeout // 10) + EMIT_ALLOWANCE_SECONDS


# THE ARCHIVED LANES (owner, 2026-10-25: "archive SaBRe and e9patch to
# nightly-only tests"; hermit ci/manifest-plan/src/archived.rs). Their tests
# are not manifest cells, so pressure-test.rs cannot run them: each lane is
# one Hermit focused validation, `scripts/validate.rs <flag>
# --allow-local-off-the-record-run`, run in the campaign checkout under the
# part's own bench admission, which is what keeps it off a running validate.
# It writes no ledger row; its durable log under the state root's
# ignored/validate/, the part's validate.log and its sweep line
# (FINAL_VALIDATE_STATUS, wall and CPU) are the record. A part on a Hermit
# whose validation graph lacks the lane's node records `unsupported` and runs
# nothing, as parity-all does.
#   sabre-examples  --sabre-compat-only: the SaBRe run type, whose
#                   sabrecompat.sabre_examples node runs the 34 tests of
#                   hermit-cli/tests/sabre_examples.rs outside the per-commit
#                   smoke pair, beside the compat bucket's SaBRe cells;
#   e9patch-corpus  --e9patch-compat-only: the e9patch corpus, which builds
#                   e9patch and e9tool through hermit-install and runs every
#                   program probe with e9patch preprocessing on ptrace.
ARCHIVED_LANES: Mapping[str, tuple[str, str]] = {
    "sabre-examples": ("--sabre-compat-only", "sabrecompat.sabre_examples"),
    "e9patch-corpus": ("--e9patch-compat-only", "e9patchcompatprep.release_resources"),
}
# Each part's whole-run bound. Measured cold in a fresh Hermit checkout on
# 2026-10-26 (hermit archive/nightly-sabre): --e9patch-compat-only took 8 m 02 s
# of wall and 2,763 CPU-s for its 181 nodes, its release builds included.
ARCHIVED_LANE_RUN_TIMEOUT_SECONDS = 7200
FINAL_STATUS_RE = re.compile(r"^FINAL_VALIDATE_STATUS: (\w+)$", re.MULTILINE)
VALIDATE_COST_RE = re.compile(r"^\s*wall (\S+) \| CPU (\S+) ", re.MULTILINE)

SWEEP_PARTS: tuple[str, ...] = (
    "red",
    "sample",
    *(f"survey-{backend}" for backend in ALL_BACKENDS),
    *ARCHIVED_LANES,
    "parity-all",
)
# THE PARITY-ALL PART (owner, 2026-10-20: "have full partial credit dataset for
# all backends for all test ids in the verify buckets"; coordinator ruling B1).
# Once a night, `pressure-test.rs run --parity-all --repetitions 1` runs every
# verify cell of every test in the parity population on ptrace and on every
# candidate backend, whatever its status, a disabled candidate as a parity probe
# on ptrace's inputs, so the series' parity post-pass measures all of them at
# the night's main and emit-series publishes the rows. It runs last: the parity
# summary shows each producer's latest run, and this is the one that should be
# pressure-test's. Measured with the equivalent scratch selection on
# 2026-10-20: 3,488 cell runs in 32 minutes at 40 guests (KVM 16), but the
# population's declared worst case needs a 28,800 s bound at that width. The
# nightly gives the part the sweep's width, at most 32 guests, where the
# planner declared 30,303 s for 3,468 runs on 2026-10-21 and refused the
# 28,800 s bound, so the part never ran. 36,000 s covers that with a fifth
# to spare for cells flipped since; the run itself still takes about half
# an hour, and only its worst case is this long.
PARITY_ALL_PART = "parity-all"
PARITY_ALL_RUN_TIMEOUT_SECONDS = 36000
PARITY_ALL_KVM_GUEST_CAP = 16
RESURVEY_PARTS: tuple[str, ...] = tuple(f"resurvey-{backend}" for backend in ALL_BACKENDS)


def part_backend(part: str) -> str | None:
    for prefix in ("survey-", "resurvey-"):
        if part.startswith(prefix) and part[len(prefix):] in ALL_BACKENDS:
            return part[len(prefix):]
    return None


def part_run_timeout(part: str) -> int:
    """The largest whole-run bound PART can take; its child deadline encloses it."""

    if part == "red":
        return RUN_TIMEOUT_SECONDS
    if part == "sample":
        return SAMPLE_RUN_TIMEOUT_SECONDS
    if part == PARITY_ALL_PART:
        return PARITY_ALL_RUN_TIMEOUT_SECONDS
    if part in ARCHIVED_LANES:
        return ARCHIVED_LANE_RUN_TIMEOUT_SECONDS
    if part_backend(part) and part.startswith("survey-"):
        return SURVEY_MAX_RUN_TIMEOUT_SECONDS
    if part_backend(part) and part.startswith("resurvey-"):
        return RESURVEY_RUN_TIMEOUT_SECONDS
    raise ValueError(f"unknown sweep part {part!r}")


def survey_run_timeout(cells: int) -> int:
    """A survey part's bound: 7,200 s, or its declared need at the narrowest width."""

    need = -(-cells * SWEEP_REPETITIONS * SURVEY_CELL_ALLOWANCE_SECONDS // SWEEP_MIN_GUESTS)
    bound = max(SURVEY_RUN_TIMEOUT_SECONDS, -(-(need + 1800) // 1800) * 1800)
    return min(bound, SURVEY_MAX_RUN_TIMEOUT_SECONDS)


LOCK_CHILD_DEADLINE_SECONDS = child_deadline(RUN_TIMEOUT_SECONDS)
# wrkslots fails immediately on a busy configuration or state lock unless given
# --wait-lock; other agents' create/remove calls hold those locks routinely.
SLOT_LOCK_WAIT_SECONDS = 900
# A night's campaign starts no part after SWEEP_LAST_PART_START_HOUR, so it ends
# within the window's span plus one part's admission wait and child deadline.
UNIT_RUNTIME_SECONDS = (
    SLOT_LOCK_WAIT_SECONDS  # wrkslots create
    + (SWEEP_LAST_PART_START_HOUR - SWEEP_WINDOW_START_HOUR) * 3600
    + LOCK_WAIT_SECONDS
    + child_deadline(
        max(part_run_timeout(part) for part in (*SWEEP_PARTS, *RESURVEY_PARTS))
    )
    + SLOT_LOCK_WAIT_SECONDS  # wrkslots remove
    + 300
)
# The systemd user manager's environment has no GIT_ALLOW_PROTOCOL, and the
# user's url.insteadOf rewrites submodule URLs to local paths, so the
# post-provision `git submodule update` fails with "transport 'file' not
# allowed".  Same remedy as the integration-tip launcher in
# https://github.com/rrnewton/dev-hermit/commit/84ff6369d7f806fdac0bf5d7962aa20f534bcae3.
UNIT_GIT_SETENV: Mapping[str, str] = {"GIT_ALLOW_PROTOCOL": "https:file:ssh"}
SHA_RE = re.compile(r"[0-9a-f]{40}")
# PATH's original git, handed to the unit only when launch() replaced PATH's
# git with the unwrapped binary. run_campaign uses it for its one network
# call, the submodule fetch, so that call keeps the telemetry wrapper and its
# retry of HTTP 429 responses, as it had before the substitution.
NETWORK_GIT_ENV = unwrapped_git.NETWORK_GIT_ENV
# THE HEALTH TICK GIVES launch() 30 SECONDS (ci-hub/health/tick-hub.yaml), SO
# launch() TAKES NO LOCK THAT IT CANNOT BOUND. Writing the run record takes the
# run-writer generation lock and the run-population lock, and the population
# lock is an exclusive flock on the state root, which is also wrkslots' own
# configuration lock. Validation cleanup holds the generation lock exclusively,
# and wrkslots holds the configuration lock, for as long as their removals take.
# On 2026-10-09 the tick's validate_checkout_cleanup worker ran from 23:32:49
# to 23:36:43Z; launch() logged its git substitution at 23:33:18Z, the step
# just before the record write, never rewrote the record, and was killed at the
# gate's 30 s bound, so no unit started and this slot's git directory was left
# behind. Sampled from /proc/locks the next half hour: the worker's `wrkslots
# retire-pending` held the population lock in 3-11 s holds, back to back for
# four minutes, and a validation cleanup (start_unit.py --publication-stopped)
# held the generation lock exclusively for 34.6 s on its own. launch() therefore
# hands the record to the unit as a plain launch intent (no lock), and the unit,
# which has RuntimeMaxSec to wait in, writes the run record before it does
# anything else. Uncontended, launch() measured 1.4 s on devbig014 that day,
# 1.2 s of it this fetch; FETCH_TIMEOUT_SECONDS bounds the fetch inside the
# gate's 30 s.
FETCH_TIMEOUT_SECONDS = 20
# THE NIGHT'S TARGET (owner, 2026-10-25). The sweep tests the newest commit of
# Hermit main that a regular full validation on this host finished canonically
# VALIDATED: the run record's canonical_verdict, never the log's
# FINAL_VALIDATE_STATUS, which can say PASSED for a run that is not. It takes
# that commit however many commits or days behind main it is, and names how far
# behind: the sweep exists to find deeper regressions that regular validation
# misses, so a breakage at main's tip must neither stop it nor hide them. (The
# night of 2026-10-25 swept main's tip 7903351d, whose first validation had
# failed.) Only when none of main's last TARGET_HISTORY_COMMITS first-parent
# commits was ever validated does it test main's tip, and it says so.
TARGET_HISTORY_COMMITS = 2000
VALIDATED_VERDICT = "VALIDATED"
EPOCH = dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def state_path(root: Path) -> Path:
    return root / "ignored" / "ci-hub" / "pressure-test-state.json"


def log_path(root: Path) -> Path:
    return root / "ignored" / "ci-hub" / "pressure-test.log"


def launch_lock_path(root: Path) -> Path:
    return root / "ignored" / "ci-hub" / "pressure-test-launch.lock"


def launch_intent_path(root: Path) -> Path:
    """The run record launch() hands to the unit; see FETCH_TIMEOUT_SECONDS.

    One fixed path: launch() writes it only while holding the launch lock with
    the unit inactive, and the unit consumes it before anything else, so no
    two writers or readers can overlap.
    """
    return root / "ignored" / "ci-hub" / "pressure-test-launch-intent.json"


def write_launch_intent(root: Path, value: Mapping[str, object]) -> Path:
    path = launch_intent_path(root)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.tmp-{os.getpid()}")
    temporary.write_text(json.dumps(dict(value), indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)
    return path


def read_launch_intent(path: Path) -> dict:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise ValueError(f"launch intent {path} is not a JSON object")
    return value


def adopt_launch_intent(intent: Path, record: Path, target: str, root: Path) -> None:
    """Write launch()'s run record from inside the unit, then consume the intent.

    This is the blocking record write launch() cannot afford. A refused intent
    (unreadable, another target, an invalid record) removes the git directory
    it names, because no campaign will run to remove it.
    """
    try:
        value = read_launch_intent(intent)
    except (OSError, ValueError) as error:
        raise RuntimeError(f"cannot read the pressure-test launch intent: {error}")
    try:
        if value.get("target") != target:
            raise RuntimeError(
                f"launch intent {intent} names target {value.get('target')!r}, "
                f"not {target}"
            )
        value["state"] = "running"
        run_registry.write_current_record(record, value)
    except Exception as error:
        cleanup_error = remove_recorded_unwrapped_git_directory(
            root, value.get("git_substitution")
        )
        raise RuntimeError(
            f"pressure-test run record not written: {error}"
            + (f" cleanup_error={cleanup_error}" if cleanup_error else "")
        ) from error
    except BaseException:
        remove_recorded_unwrapped_git_directory(root, value.get("git_substitution"))
        raise
    finally:
        intent.unlink(missing_ok=True)


def unwrapped_git_root(root: Path) -> Path:
    """Where every pressure test's private git directory lives, one per slot."""
    return root / "ignored" / "ci-hub" / "pressure-test-git"


def unwrapped_git_directory(root: Path, slot: str) -> Path:
    """This run's private PATH entry holding only a `git` symlink."""
    return unwrapped_git_root(root) / slot


def network_git(environment: Mapping[str, str] | None = None) -> str:
    """The git for the campaign's submodule fetch: see NETWORK_GIT_ENV."""
    environment = os.environ if environment is None else environment
    value = environment.get(NETWORK_GIT_ENV, "")
    return value if os.path.isabs(value) else "git"


def remove_unwrapped_git_directory(
    root: Path, slot: str, substitution: object
) -> str | None:
    """Remove this slot's git directory when launch() created it.

    `substitution` is the run record's `git_substitution`. Nothing is touched
    unless it records an applied substitution whose directory is exactly this
    slot's; unwrapped_git.remove then deletes only a directory whose only
    entry is a `git` symlink to the recorded binary.
    """
    if not isinstance(substitution, Mapping) or substitution.get("applied") is not True:
        return None
    directory = unwrapped_git_directory(root, slot)
    if substitution.get("directory") != str(directory):
        return (
            f"run record names git directory {substitution.get('directory')!r}, "
            f"not {directory}; left in place"
        )
    real_git = substitution.get("real_git")
    if not isinstance(real_git, str) or not real_git:
        return f"run record names no unwrapped git for {directory}; left in place"
    return unwrapped_git.remove(directory, real_git)


def remove_recorded_unwrapped_git_directory(
    root: Path, substitution: object
) -> str | None:
    """remove_unwrapped_git_directory for a run record that names no slot.

    The slot is taken from the recorded directory, which must be a direct
    child of unwrapped_git_root; anything else is left in place.
    """
    if not isinstance(substitution, Mapping) or substitution.get("applied") is not True:
        return None
    recorded = substitution.get("directory")
    if not isinstance(recorded, str) or Path(recorded).parent != unwrapped_git_root(root):
        return (
            f"run record names git directory {recorded!r}, which is not a slot "
            f"under {unwrapped_git_root(root)}; left in place"
        )
    return remove_unwrapped_git_directory(root, Path(recorded).name, substitution)


def sweep_unwrapped_git_directories(root: Path) -> tuple[list[str], list[str]]:
    """Remove every leftover slot directory; returns (removed, problems).

    Only launch() calls this, holding the launch lock after systemd reports
    the pressure unit inactive: the directories belong to that unit's
    campaign, so none is in use. It catches what run_campaign could not
    remove itself, for example after the unit was killed at RuntimeMaxSec.
    Each entry goes through unwrapped_git.remove with the host's unwrapped
    git, so only the exact shape launch() creates is deleted. Anything else
    (another link target, extra entries, a file) is left in place and named
    in `problems`.
    """
    parent = unwrapped_git_root(root)
    try:
        names = sorted(os.listdir(parent))
    except FileNotFoundError:
        return [], []
    except OSError as error:
        return [], [f"cannot list {parent}: {error}"]
    removed: list[str] = []
    problems: list[str] = []
    for name in names:
        problem = unwrapped_git.remove(parent / name, unwrapped_git.REAL_GIT)
        if problem is None:
            removed.append(name)
        else:
            problems.append(problem)
    return removed, problems


def emit(state: str, summary: str) -> None:
    print(f"state={state}")
    print(f"summary={summary}")


def write_state(root: Path, value: dict) -> None:
    path = state_path(root)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.tmp-{os.getpid()}")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    os.replace(temporary, path)


def read_state(root: Path) -> dict | None:
    try:
        value = json.loads(state_path(root).read_text())
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def launch_command(
    root: Path,
    systemd_run: str,
    with_proxy: str,
    target: str,
    record: Path,
    environment: Mapping[str, str] | None = None,
    placement: cpu_placement.CpuPlacement | None = None,
    path: str | None = None,
    network_git_path: str | None = None,
    launch_intent: Path | None = None,
) -> list[str]:
    environment = os.environ if environment is None else environment
    if path is None:
        path = environment.get("PATH", "")
    # Same placement as validate-run (https://github.com/rrnewton/hermit/issues/3265).
    # A malformed DEV_HERMIT_EXCLUDE_CPUS raises CpuPlacementError, a ValueError,
    # which launch() reports as a refusal before any unit exists.
    if placement is None:
        placement = cpu_placement.derive(environ=environment)
    log = log_path(root)
    operational_tool = environment.get("DEV_HERMIT_OPERATIONAL_TOOL", "")
    parent_sha = environment.get("DEV_HERMIT_TOOL_PARENT_SHA", "")
    cache_root = environment.get("DEV_HERMIT_TOOL_CACHE", "")
    has_handoff = bool(operational_tool or parent_sha or cache_root)
    if has_handoff:
        if (
            not operational_tool
            or not os.path.isabs(operational_tool)
            or not os.path.isfile(operational_tool)
            or not os.access(operational_tool, os.X_OK)
            or SHA_RE.fullmatch(parent_sha) is None
            or not cache_root
            or not os.path.isabs(cache_root)
            or not os.path.isdir(cache_root)
        ):
            raise ValueError(
                "incomplete operational-tool handoff; recurring pressure test "
                "not launched"
            )
        working_directory = root
        worker = [
            operational_tool,
            "--run-current-tool",
            "--state-root",
            str(root),
            "--cache-root",
            cache_root,
            "--tool-parent-sha",
            parent_sha,
            "--tool-relative",
            "ci-hub/series/pressure_schedule.py",
            "--",
        ]
    else:
        working_directory = SOURCE_ROOT
        worker = [
            sys.executable,
            str(SOURCE_ROOT / "ci-hub" / "series" / "pressure_schedule.py"),
        ]

    return [
        systemd_run,
        "--user",
        "--quiet",
        "--collect",
        f"--unit={UNIT}",
        "--description=run one recurring Hermit pressure-test sample",
        f"--property=RuntimeMaxSec={UNIT_RUNTIME_SECONDS}",
        f"--property=StandardOutput=append:{log}",
        f"--property=StandardError=append:{log}",
        *(
            [f"--property={placement.systemd_property()}"]
            if placement.systemd_property()
            else []
        ),
        f"--working-directory={working_directory}",
        f"--setenv=PATH={path}",
        *(
            [f"--setenv={NETWORK_GIT_ENV}={network_git_path}"]
            if network_git_path
            else []
        ),
        "--setenv=PYTHONUNBUFFERED=1",
        f"--setenv=DEV_HERMIT_PARENT={root}",
        f"--setenv=DEV_HERMIT_TOOL_ROOT={SOURCE_ROOT}",
        *(
            f"--setenv={name}={value}"
            for name, value in sorted(UNIT_GIT_SETENV.items())
        ),
        # pressure-test.rs re-executes into a dagrun scope in another slice,
        # where AllowedCPUs above no longer applies; safe_ci_scope reads these
        # and applies the same list to that scope.
        *(
            f"--setenv={key}={value}"
            for key, value in placement.environment().items()
        ),
        "--",
        with_proxy,
        *worker,
        "_run",
        "--target",
        target,
        "--record",
        str(record),
        "--state-root",
        str(root),
        *(["--launch-intent", str(launch_intent)] if launch_intent else []),
    ]


def locked_command(
    root: Path,
    checkout: Path,
    target: str,
    results: Path,
    record: Path,
    part: str = "red",
) -> list[str]:
    return [
        str(SOURCE_ROOT / "ci-hub" / "ci-hub"),
        "validate-lock",
        "run",
        "--agent",
        "tick-pressure-test",
        "--kind",
        "bench",
        "--target",
        target,
        "--run-record",
        str(record),
        "--wait",
        str(LOCK_WAIT_SECONDS),
        "--hold",
        "600",
        "--child-deadline",
        str(child_deadline(part_run_timeout(part))),
        "--",
        sys.executable,
        str(SOURCE_ROOT / "ci-hub" / "series" / "pressure_schedule.py"),
        "_locked",
        "--checkout",
        str(checkout),
        "--results",
        str(results),
        "--state-root",
        str(root),
        "--part",
        part,
    ]


def part_results(results: Path, part: str) -> Path:
    """The red part keeps the campaign's directory; the others sit beside it."""

    return results if part == "red" else results.with_name(f"{results.name}-{part}")


def sample_commands(checkout: Path, results: Path) -> list[list[str]]:
    pressure = str(checkout / "ci" / "compat-envelope" / "pressure-test.rs")
    return [
        [
            pressure,
            "run",
            "--results",
            str(results),
            "--sample",
            str(SAMPLE_COUNT),
            "--jobs",
            "4",
            "--run-timeout",
            str(SAMPLE_RUN_TIMEOUT_SECONDS),
        ],
        [pressure, "emit-series", "--results", str(results)],
    ]


CELL_KEYS = ("backend", "category", "lane", "mode", "test")


def promoted_path(root: Path) -> Path:
    """Cells a weekly re-survey moved into the nightly survey population."""

    return root / "ignored" / "ci-hub" / "sweep-promoted.jsonl"


def cell_key(cell: Mapping[str, str]) -> str:
    return json.dumps({key: cell[key] for key in CELL_KEYS}, sort_keys=True)


def promoted_cells(root: Path) -> set[str]:
    path = promoted_path(root)
    if not path.is_file():
        return set()
    return {
        cell_key(json.loads(line)["cell"])
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    }


def scorecard_cells(checkout: Path) -> list[dict]:
    path = checkout / "ci" / "compat-envelope" / "cells.json"
    return json.loads(path.read_text(encoding="utf-8"))["cells"]


CELL_FIELDS = ("lane", "category", "test", "mode", "backend")

# The red part's one named exclusion (coordinator, 2026-10-19). A naked cell on
# the native backend is the uninstrumented baseline, and the sweep's hermetic
# validation environment refuses it before it runs; on the first night every
# such cell ended INCOMPLETE with this refusal and no observation. The sweep
# report names the excluded cells and this reason, so they are not silently gone.
NATIVE_NAKED_EXCLUSION = (
    "hermetic validation cannot isolate a naked cell on the native backend at /test"
)


def red_cells(checkout: Path) -> tuple[list[dict], list[dict]]:
    """The red part's cells, and the naked@native cells it leaves out."""

    selected, excluded = [], []
    for cell in scorecard_cells(checkout):
        if cell.get("status") != "red":
            continue
        identity = {field: cell[field] for field in CELL_FIELDS}
        if identity["mode"] == "naked" and identity["backend"] == "native":
            excluded.append(identity)
        else:
            selected.append(identity)
    return selected, excluded


def survey_cells(checkout: Path, backend: str, root: Path | None = None) -> list[dict]:
    """BACKEND's nightly survey population, from the checkout's scorecard.

    Not-applicable cells whose reason records a failed survey, or that a weekly
    re-survey promoted (under ROOT), and never an owner-held one.
    """

    promoted = promoted_cells(root) if root is not None else set()
    chosen = []
    for cell in scorecard_cells(checkout):
        reason = cell.get("not_applicable_reason") or ""
        if (
            cell.get("backend") == backend
            and cell.get("status") == "not-applicable"
            and not OWNER_HELD_RE.search(reason)
            and (SURVEY_FAILED_RE.search(reason) or cell_key(cell) in promoted)
        ):
            chosen.append({key: cell[key] for key in CELL_KEYS})
    return sorted(chosen, key=lambda cell: json.dumps(cell, sort_keys=True))


def promote(root: Path, checkout: Path, results: Path, night: str) -> int:
    """Add RESULTS' CLEAN, not owner-held, not-applicable cells to the promoted list."""

    summary = json.loads((results / "summary.json").read_text(encoding="utf-8"))
    reasons = {
        cell_key(cell): cell.get("not_applicable_reason") or ""
        for cell in scorecard_cells(checkout)
        if cell.get("status") == "not-applicable"
    }
    known = promoted_cells(root)
    added = []
    for entry in summary["repeated_cells"]:
        key = cell_key(entry["cell"])
        if (
            entry.get("verdict") == "CLEAN"
            and key in reasons
            and not OWNER_HELD_RE.search(reasons[key])
            and key not in known
        ):
            added.append(
                {"cell": json.loads(key), "night": night, "run": results.name}
            )
            known.add(key)
    if added:
        path = promoted_path(root)
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as promoted:
            for entry in added:
                promoted.write(json.dumps(entry, sort_keys=True) + "\n")
    return len(added)


def write_cells_file(path: Path, cells: Sequence[Mapping[str, str]]) -> None:
    path.write_text(
        "".join(
            json.dumps(cell, sort_keys=True, separators=(",", ":")) + "\n"
            for cell in cells
        ),
        encoding="utf-8",
    )


def survey_commands(
    checkout: Path,
    results: Path,
    backend: str,
    cells_file: Path | None,
    guests: int,
    run_timeout: int,
    repetitions: int = SWEEP_REPETITIONS,
) -> list[list[str]]:
    """A --probe-disabled run of BACKEND: the CELLS_FILE population, or every
    executable disabled cell when CELLS_FILE is None (the weekly re-survey)."""
    pressure = str(checkout / "ci" / "compat-envelope" / "pressure-test.rs")
    return [
        [
            pressure,
            "run",
            "--results",
            str(results),
            "--probe-disabled",
            "--backend",
            backend,
            *(["--cells-file", str(cells_file)] if cells_file is not None else []),
            "--repetitions",
            str(repetitions),
            "--no-retry",
            "--jobs",
            str(guests),
            "--manifest-guest-cap",
            str(guests),
            "--run-timeout",
            str(run_timeout),
        ],
        [pressure, "emit-series", "--results", str(results)],
    ]


def parity_all_supported(checkout: Path) -> bool:
    """Whether the checkout's pressure-test.rs has `--parity-all` (hermit since
    the B1 change); an older one has nothing for this part to run."""
    try:
        source = (checkout / "ci" / "compat-envelope" / "pressure-test.rs").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return False
    return '"--parity-all"' in source


def parity_all_commands(
    checkout: Path, results: Path, guests: int, run_timeout: int
) -> list[list[str]]:
    pressure = str(checkout / "ci" / "compat-envelope" / "pressure-test.rs")
    return [
        [
            pressure,
            "run",
            "--results",
            str(results),
            "--parity-all",
            "--repetitions",
            "1",
            "--jobs",
            str(guests),
            "--manifest-guest-cap",
            str(guests),
            "--kvm-guest-cap",
            str(min(PARITY_ALL_KVM_GUEST_CAP, guests)),
            "--run-timeout",
            str(run_timeout),
        ],
        [pressure, "emit-series", "--results", str(results)],
    ]


def pressure_commands(
    checkout: Path, results: Path, guests: int, cells_file: Path
) -> list[list[str]]:
    pressure = str(checkout / "ci" / "compat-envelope" / "pressure-test.rs")
    return [
        [
            pressure,
            "run",
            "--results",
            str(results),
            "--cells-file",
            str(cells_file),
            "--repetitions",
            str(SWEEP_REPETITIONS),
            "--no-retry",
            "--jobs",
            str(guests),
            "--manifest-guest-cap",
            str(guests),
            "--run-timeout",
            str(RUN_TIMEOUT_SECONDS),
        ],
        [pressure, "emit-series", "--results", str(results)],
    ]


def archived_lane_supported(checkout: Path, part: str) -> bool:
    """Whether the checkout's validation graph has PART's lane node; a Hermit
    from before the archival has nothing for the part to run."""
    _, tag = ARCHIVED_LANES[part]
    try:
        graph = json.loads(
            (checkout / "ci" / "dag" / "validate.json").read_text(encoding="utf-8")
        )
    except (OSError, ValueError):
        return False
    return any(
        f"{step.get('group')}.{step.get('job')}" == tag
        for step in graph.get("steps", [])
        if isinstance(step, dict)
    )


def archived_lane_command(checkout: Path, part: str, run_timeout: int) -> list[str]:
    flag, _ = ARCHIVED_LANES[part]
    return [
        str(checkout / "scripts" / "validate.rs"),
        flag,
        "--allow-local-off-the-record-run",
        "--no-label-pr",
        "--run-timeout",
        str(run_timeout),
    ]


def duration_seconds(text: str) -> float | None:
    """validate.rs's summary durations: 52s, 1m43s, 2h5m3s."""
    match = re.fullmatch(r"(?:(\d+)h)?(?:(\d+)m)?(?:(\d+(?:\.\d+)?)s)?", text)
    if not text or match is None:
        return None
    hours, minutes, seconds = (float(group or 0) for group in match.groups())
    return hours * 3600 + minutes * 60 + seconds


def validate_outcome(log: str) -> dict:
    """The final status and validate.rs's own wall and CPU, from its log."""
    statuses = FINAL_STATUS_RE.findall(log)
    costs = VALIDATE_COST_RE.findall(log)
    outcome: dict[str, object] = {"final_status": statuses[-1] if statuses else None}
    if costs:
        wall, cpu = costs[-1]
        outcome["validate_wall_s"] = duration_seconds(wall)
        outcome["validate_cpu_s"] = duration_seconds(cpu)
    return outcome


def run_archived_lane(
    checkout: Path,
    results: Path,
    part: str,
    sweep: Mapping[str, object],
    root: Path,
    environment: Mapping[str, str],
) -> int:
    """One archived lane (ARCHIVED_LANES): validate.rs's exit status is the part's."""
    if not archived_lane_supported(checkout, part):
        _, tag = ARCHIVED_LANES[part]
        print(
            f"pressure_schedule: {part}: this Hermit's ci/dag/validate.json has no {tag}; "
            "nothing to run",
            file=sys.stderr,
        )
        append_sweep(root, {**sweep, "result": "unsupported", "finished_at": utc_now()})
        return 0
    command = archived_lane_command(checkout, part, part_run_timeout(part))
    results.mkdir(parents=True, exist_ok=True)
    log = results / "validate.log"
    started = time.monotonic()
    with log.open("w", encoding="utf-8") as output:
        ran = subprocess.run(
            command,
            cwd=checkout,
            env=dict(environment),
            stdout=output,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            check=False,
        )
    wall = round(time.monotonic() - started, 1)
    try:
        text = log.read_text(encoding="utf-8", errors="replace")
    except OSError:
        text = ""
    outcome = validate_outcome(text)
    append_sweep(
        root,
        {
            **sweep,
            "result": "lane",
            "lane": command[1],
            "validate_exit": ran.returncode,
            **outcome,
            "wall_s": wall,
            "log": str(log),
            "finished_at": utc_now(),
        },
    )
    print(
        f"pressure_schedule: {part}: validate.rs {command[1]} exit {ran.returncode}, "
        f"FINAL_VALIDATE_STATUS {outcome['final_status']}, {wall:.0f} s; log {log}",
        file=sys.stderr,
    )
    return ran.returncode


def sweeps_path(root: Path) -> Path:
    """One JSON line per nightly sweep, for sweep_report.py."""

    return root / "ignored" / "ci-hub" / "pressure-sweeps.jsonl"


def parse_utc(text: object) -> dt.datetime | None:
    if not isinstance(text, str):
        return None
    try:
        return dt.datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        return None


def sweep_now() -> dt.datetime:
    # Tests that run the gate's command line pin the clock here.
    return parse_utc(os.environ.get(SWEEP_CLOCK_ENV)) or dt.datetime.now(
        dt.timezone.utc
    )


def part_too_late(campaign_started: dt.datetime, now: dt.datetime) -> bool:
    """Whether a night's campaign may no longer start another part."""

    started = campaign_started.astimezone(SWEEP_ZONE)
    if not SWEEP_WINDOW_START_HOUR <= started.hour < SWEEP_WINDOW_END_HOUR:
        return False
    cutoff = started.replace(
        hour=SWEEP_LAST_PART_START_HOUR, minute=0, second=0, microsecond=0
    )
    return now.astimezone(SWEEP_ZONE) >= cutoff


def part_is_empty(checkout: Path, part: str, root: Path) -> bool:
    """Whether a nightly survey part has no cell to run, known before it asks
    for the box. Any other part, or one whose population cannot be read here,
    goes through admission and run_locked decides."""

    backend = part_backend(part)
    if backend is None or part.startswith("resurvey-"):
        return False
    try:
        return not survey_cells(checkout, backend, root)
    except Exception:  # run_locked reads it again under the lock, and reports
        return False


def night_parts(campaign_started: dt.datetime) -> tuple[str, ...]:
    """Tonight's parts: the nightly ones, and the weekly re-survey on Sunday's night."""

    started = campaign_started.astimezone(SWEEP_ZONE)
    weekly = (
        SWEEP_WINDOW_START_HOUR <= started.hour < SWEEP_WINDOW_END_HOUR
        and started.weekday() == RESURVEY_WEEKDAY
    )
    return SWEEP_PARTS + (RESURVEY_PARTS if weekly else ())


def sweep_due(root: Path, now: dt.datetime | None = None) -> tuple[bool, str]:
    """Whether tonight's sweep should launch now, and why not if it should not.

    Due inside [01:00, 06:00) US Eastern unless a sweep already started in
    tonight's window, as the state file or a launch intent the unit has not
    adopted yet records it. An intent systemd refused did not start a sweep.
    """

    now = now or sweep_now()
    local = now.astimezone(SWEEP_ZONE)
    if not SWEEP_WINDOW_START_HOUR <= local.hour < SWEEP_WINDOW_END_HOUR:
        return False, (
            f"the nightly sweep runs from {SWEEP_WINDOW_START_HOUR:02d}:00 to "
            f"{SWEEP_WINDOW_END_HOUR:02d}:00 US Eastern; it is "
            f"{local.strftime('%H:%M')}"
        )
    window = local.replace(
        hour=SWEEP_WINDOW_START_HOUR, minute=0, second=0, microsecond=0
    )
    starts = []
    state = read_state(root) or {}
    starts.append(parse_utc(state.get("started_at")))
    try:
        intent = read_launch_intent(launch_intent_path(root))
    except (OSError, ValueError):
        intent = {}
    if intent.get("state") != "refused":
        starts.append(parse_utc(intent.get("started_at")))
    started = max((start for start in starts if start), default=None)
    if started is not None and started >= window:
        return False, (
            "tonight's sweep already started at "
            f"{started.astimezone(SWEEP_ZONE).strftime('%H:%M')} US Eastern"
        )
    return True, ""


def read_cpu_times(stat: Path = Path("/proc/stat")) -> tuple[int, int]:
    """(idle + iowait, total) jiffies of the host's aggregate cpu line."""

    fields = stat.read_text().splitlines()[0].split()
    if fields[0] != "cpu":
        raise ValueError(f"{stat} does not start with the aggregate cpu line")
    values = [int(value) for value in fields[1:9]]
    return values[3] + values[4], sum(values)


def idle_cores(
    seconds: float = SWEEP_IDLE_SAMPLE_SECONDS,
    read: Callable[[], tuple[int, int]] = read_cpu_times,
    sleep: Callable[[float], None] = time.sleep,
    cpus: int | None = None,
) -> tuple[float, int]:
    """Idle cores over SECONDS, from /proc/stat, and the host's core count."""

    cpus = cpus or os.cpu_count() or 1
    idle_before, total_before = read()
    sleep(seconds)
    idle_after, total_after = read()
    elapsed = total_after - total_before
    if elapsed <= 0:
        raise ValueError("/proc/stat did not advance during the idle sample")
    return cpus * (idle_after - idle_before) / elapsed, cpus


def sweep_guests(idle: float) -> int:
    return int(idle * SWEEP_IDLE_SHARE)


UNSIZED_GUESTS = 32
SAFE_CAP_RES = (
    re.compile(r"highest safe caps at -j \d+: manifest=(\d+)"),
    re.compile(r"highest safe cap for this population at -j \d+ is (\d+)"),
)


def memory_safe_guests(
    run_command: Sequence[str],
    guests: int,
    results: Path,
    checkout: Path,
    environment: Mapping[str, str],
) -> int:
    """GUESTS, or less when pressure-test.rs's `plan` of the same selection
    reports a lower highest memory-safe manifest-guest cap: the declared memory
    caps of that many concurrent guests must fit the observed memory budget,
    which `run` would otherwise refuse after its checkout is made. A plan that
    reports no cap gives the width every night ran at before this sizing,
    UNSIZED_GUESTS, rather than a width nothing has checked."""

    # The plan's directory is the campaign checkout's, removed with it, never
    # beside the results, where a reader of ignored/compat-envelope would take
    # a plan for a run.
    scratch = checkout / "ignored" / f"width-plan-{results.name}"
    plan = [run_command[0], "plan"]
    words = iter(run_command[2:])
    for word in words:
        if word == "--results":
            next(words, None)
            plan += ["--results", str(scratch)]
        elif word != REUSE_FRESH_FLAG:
            plan.append(word)
    try:
        planned = subprocess.run(
            plan, cwd=checkout, env=environment, capture_output=True, text=True, check=False
        )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    text = f"{planned.stdout or ''}\n{planned.stderr or ''}"
    for pattern in SAFE_CAP_RES:
        found = pattern.search(text)
        if found:
            return max(1, min(guests, int(found.group(1))))
    return min(guests, UNSIZED_GUESTS)


def with_width(command: Sequence[str], guests: int) -> list[str]:
    """COMMAND with its --jobs and --manifest-guest-cap set to GUESTS, and a
    --kvm-guest-cap no larger than GUESTS."""

    out = list(command)
    for flag in ("--jobs", "--manifest-guest-cap"):
        if flag in out:
            out[out.index(flag) + 1] = str(guests)
    if "--kvm-guest-cap" in out:
        at = out.index("--kvm-guest-cap") + 1
        out[at] = str(min(int(out[at]), guests))
    return out


# BUILD ONCE A NIGHT. Each part is its own pressure-test run, and each run
# cloned and built the measured commit afresh: on the night of 2026-10-25 the
# 13 parts that ran built 7903351d 13 times, about 540 s of wall and 5,700
# CPU-s each, 74% of the night's step CPU. A pressure-test.rs that has
# --reuse-fresh-checkout keeps one generated checkout per commit in the
# campaign checkout's ignored/, and a later part reuses it, its build with it,
# while git status there reports nothing; the campaign checkout and that
# checkout are removed together when the night ends. An older pressure-test.rs
# builds per part as before.
REUSE_FRESH_FLAG = "--reuse-fresh-checkout"


def reuse_fresh_supported(checkout: Path) -> bool:
    try:
        source = (checkout / "ci" / "compat-envelope" / "pressure-test.rs").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return False
    return f'"{REUSE_FRESH_FLAG}"' in source


REUSABLE_CHECKOUT_PREFIX = "pressure-fresh-reusable-"


def remove_reusable_checkouts(checkout: Path) -> str | None:
    """Delete the night's kept pressure-test checkouts before its slot goes.

    wrkslots refuses to remove a slot whose ignored files hold a Git
    repository with submodule repositories, even a completed validation's
    (agent-utils wrkslots _judge_nested_git_repository: "repository has
    submodule repositories under .git/modules"), and each checkout that
    --reuse-fresh-checkout keeps is a clone with Hermit's submodules
    initialized. Only directories pressure-test.rs marked as its generated
    checkouts are removed; anything else is named and left.
    """

    ignored = checkout / "ignored"
    errors = []
    try:
        entries = sorted(ignored.glob(f"{REUSABLE_CHECKOUT_PREFIX}*"))
    except OSError as error:
        return f"cannot list {ignored}: {error}"
    for path in entries:
        try:
            if path.is_symlink():
                errors.append(f"{path} is a symbolic link; left in place")
            elif path.is_file() and path.name.endswith(".lock"):
                path.unlink()
            elif path.is_dir():
                marker = path / ".git" / "pressure-test-generated-checkout"
                if not marker.is_file() or not marker.read_text(
                    encoding="utf-8", errors="replace"
                ).startswith("source="):
                    errors.append(
                        f"{path} has no pressure-test generated-checkout marker; left in place"
                    )
                else:
                    shutil.rmtree(path)
        except OSError as error:
            errors.append(f"cannot remove {path}: {error}")
    return "; ".join(errors) or None


def with_reuse(command: Sequence[str], checkout: Path) -> list[str]:
    out = list(command)
    if reuse_fresh_supported(checkout) and REUSE_FRESH_FLAG not in out:
        out.insert(2, REUSE_FRESH_FLAG)
    return out


SWEEP_VERDICTS = ("CLEAN", "FLAKY", "FAILING", "INCOMPLETE")


def unjudged(results: Path) -> str | None:
    """Why RESULTS does not judge every swept cell, or None when it does.

    pressure-test.rs fails a repeated run whenever any cell is short of CLEAN
    ("flaky is red"), which a sweep of red cells always is. A run whose
    summary gives every cell a repeated verdict completed; anything else did
    not, and stays a failure.
    """

    path = results / "summary.json"
    try:
        summary = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        return f"no readable {path}: {error}"
    cells = summary.get("repeated_cells")
    if not isinstance(cells, list) or not cells:
        return f"{path} judges no repeated cell"
    unknown = [cell for cell in cells if cell.get("verdict") not in SWEEP_VERDICTS]
    if unknown:
        return f"{path} leaves {len(unknown)} of {len(cells)} cell(s) without a repeated verdict"
    return None


def append_sweep(root: Path, value: Mapping[str, object]) -> None:
    path = sweeps_path(root)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as sweeps:
        sweeps.write(json.dumps(value, sort_keys=True) + "\n")


def sweep_line_count(root: Path) -> int:
    try:
        return len(sweeps_path(root).read_text(encoding="utf-8").splitlines())
    except FileNotFoundError:
        return 0


def parse_finished(text: object) -> dt.datetime | None:
    """A run record's finished_at: run_registry writes ISO 8601 with an offset
    and microseconds (2026-10-25T08:26:34.661853+00:00); older records use Z."""

    if not isinstance(text, str) or not text:
        return None
    try:
        moment = dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        return None
    return moment if moment.tzinfo else moment.replace(tzinfo=dt.timezone.utc)


def validated_targets(root: Path) -> dict[str, dict[str, str]]:
    """Each Hermit commit that a regular full validation on this host finished
    canonically VALIDATED, with its latest such run: commit -> {"run", "finished_at"}.

    Every record in the run registry is read as plain JSON; one that does not
    parse, or is not a full validation of Hermit, is not evidence and is skipped.
    """

    found: dict[str, dict[str, str]] = {}
    for path in sorted((root / "ignored" / "validate" / "runs").glob("*.json")):
        if path.name.endswith(".service-result.json"):
            continue
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if not isinstance(value, dict):
            continue
        target = value.get("target")
        if (
            value.get("kind") != "validate"
            or value.get("repo") != "rrnewton/hermit"
            or value.get("selection_mode") != "full"
            or value.get("canonical_verdict") != VALIDATED_VERDICT
            or not isinstance(target, str)
            or SHA_RE.fullmatch(target) is None
        ):
            continue
        finished = str(value.get("finished_at") or "")
        moment = parse_finished(finished) or EPOCH
        if target not in found or moment > (
            parse_finished(found[target]["finished_at"]) or EPOCH
        ):
            found[target] = {"run": path.stem, "finished_at": finished}
    return found


def main_history(root: Path) -> list[str]:
    """Hermit main's first-parent commits, newest first, as last fetched; empty
    when they cannot be read."""

    try:
        listed = subprocess.run(
            [
                "git",
                "-C",
                str(root / "hermit"),
                "rev-list",
                "--first-parent",
                f"--max-count={TARGET_HISTORY_COMMITS}",
                "origin/main",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=FETCH_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if listed.returncode != 0:
        return []
    commits = listed.stdout.split()
    return commits if all(SHA_RE.fullmatch(commit) for commit in commits) else []


def choose_target(
    tip: str, history: Sequence[str], validated: Mapping[str, Mapping[str, str]]
) -> dict[str, object]:
    """The night's target: the newest of main's commits (history, newest first,
    starting at tip) that a regular full validation passed, or main's tip,
    unvalidated, when none did."""

    for behind, commit in enumerate(history):
        run = validated.get(commit)
        if run is not None:
            return {
                "target": commit,
                "main": tip,
                "validated": True,
                "commits_behind_main": behind,
                "validated_by": run["run"],
                "validated_at": run["finished_at"],
            }
    return {
        "target": tip,
        "main": tip,
        "validated": False,
        "commits_behind_main": 0,
        "validated_by": None,
        "validated_at": None,
    }


def target_validation(
    root: Path, checkout: Path, history: Callable[[Path], list[str]] = main_history
) -> dict[str, object] | None:
    """The night's target (the checkout's HEAD) as choose_target describes it:
    whether a regular full validation passed it, and how far behind main it is.
    None when the checkout's HEAD cannot be read."""

    head = subprocess.run(
        ["git", "-C", str(checkout), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    target = head.stdout.strip()
    if head.returncode != 0 or SHA_RE.fullmatch(target) is None:
        return None
    commits = history(root)
    tip = commits[0] if commits else target
    behind = commits.index(target) if target in commits else None
    run = validated_targets(root).get(target)
    return {
        "target": target,
        "main": tip,
        "validated": run is not None,
        "commits_behind_main": behind,
        "validated_by": run["run"] if run else None,
        "validated_at": run["finished_at"] if run else None,
    }


def describe_target(choice: Mapping[str, object], now: dt.datetime | None = None) -> str:
    target = str(choice["target"])[:12]
    if not choice["validated"]:
        return (
            f"target {target} is Hermit main's tip, UNVALIDATED: no regular full "
            f"validation of main's last {TARGET_HISTORY_COMMITS} first-parent commits "
            "finished VALIDATED on this host"
        )
    validated_at = parse_finished(choice["validated_at"])
    age = (
        f"{((now or dt.datetime.now(dt.timezone.utc)) - validated_at).total_seconds() / 3600:.1f} h ago"
        if validated_at
        else "at an unrecorded time"
    )
    return (
        f"target {target} validated by {choice['validated_by']} {age}, "
        f"{choice['commits_behind_main']} commit(s) behind Hermit main "
        f"{str(choice['main'])[:12]}"
    )


def launch(
    root: Path = STATE_ROOT,
    run: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    history: Callable[[Path], list[str]] = main_history,
) -> int:
    # A declared measurement window (ci-hub/lib/measurement_hold.py) holds a new
    # sample. Exit 0, not 75: tick-hub turns 75 into a counted ACTION on every
    # tick, so a sample due inside a window waits for the next daily cadence.
    holds, problems = measurement_hold.active_holds(root)
    if holds:
        emit("held", measurement_hold.hold_text(holds) + "; recurring pressure test not launched")
        return 0
    if problems:
        print(f"hold_problems={measurement_hold.problem_text(problems)}")
    log = log_path(root)
    log.parent.mkdir(parents=True, exist_ok=True)
    systemctl = shutil.which("systemctl")
    systemd_run = shutil.which("systemd-run")
    with_proxy = shutil.which("with-proxy")
    missing = [
        name
        for name, value in (
            ("systemctl", systemctl),
            ("systemd-run", systemd_run),
            ("with-proxy", with_proxy),
        )
        if value is None
    ]
    if missing:
        emit(
            "unavailable",
            "recurring pressure test not launched; missing " + ",".join(missing),
        )
        return 1

    lock_path = launch_lock_path(root)
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as launch_lock:
        try:
            fcntl.flock(launch_lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            emit(
                "running",
                f"recurring pressure test launch already in progress as {UNIT}",
            )
            return 0

        active = run(
            [systemctl, "--user", "is-active", "--quiet", f"{UNIT}.service"],
            capture_output=True,
            text=True,
            check=False,
        )
        if active.returncode == 0:
            emit("running", f"recurring pressure test already active as {UNIT}")
            return 0
        if active.returncode not in (3, 4):
            detail = (
                active.stderr or active.stdout
            ).strip() or f"exit {active.returncode}"
            emit(
                "unavailable",
                "recurring pressure test unit state could not be read: " + detail,
            )
            return 1

        # The unit is inactive and this process holds the launch lock, so no
        # campaign can be using a pressure-test-git directory.
        swept, sweep_problems = sweep_unwrapped_git_directories(root)
        sweep_error = "; ".join(sweep_problems) or None
        if swept or sweep_problems:
            with log.open("a", encoding="utf-8") as retained:
                print(
                    f"{utc_now()} pressure_schedule: swept "
                    f"{unwrapped_git_root(root)}: removed={swept} "
                    f"left={sweep_problems}",
                    file=retained,
                )

        def with_sweep(summary: str) -> str:
            return summary + (f" cleanup_error={sweep_error}" if sweep_error else "")

        try:
            fetched = run(
                [
                    with_proxy,
                    "git",
                    "-C",
                    str(root / "hermit"),
                    "fetch",
                    "--no-tags",
                    "origin",
                    "main",
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=FETCH_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired:
            emit(
                "refused",
                with_sweep(
                    "recurring pressure test could not fetch Hermit main: "
                    f"timed out after {FETCH_TIMEOUT_SECONDS}s"
                ),
            )
            return 1
        if fetched.returncode != 0:
            detail = (fetched.stderr or fetched.stdout).strip() or "no output"
            emit(
                "refused",
                with_sweep(
                    f"recurring pressure test could not fetch Hermit main: {detail}"
                ),
            )
            return 1
        resolved = run(
            ["git", "-C", str(root / "hermit"), "rev-parse", "origin/main"],
            capture_output=True,
            text=True,
            check=False,
        )
        tip = resolved.stdout.strip()
        if resolved.returncode != 0 or re.fullmatch(r"[0-9a-f]{40}", tip) is None:
            detail = resolved.stderr.strip() or tip or f"exit {resolved.returncode}"
            emit(
                "refused",
                with_sweep(
                    f"recurring pressure test could not resolve Hermit main: {detail}"
                ),
            )
            return 1
        commits = history(root)
        if commits[:1] != [tip]:
            commits = [tip]
        target_choice = choose_target(tip, commits, validated_targets(root))
        target = str(target_choice["target"])
        target_text = describe_target(target_choice)
        with log.open("a", encoding="utf-8") as retained:
            print(f"{utc_now()} pressure_schedule: {target_text}", file=retained)

        record = run_registry.record_path(root, UNIT)
        slot = (
            f"pressure-{target[:12]}-"
            f"{dt.datetime.now(dt.timezone.utc).strftime('%Y%m%d%H%M%S')}-"
            f"{secrets.token_hex(4)}"
        )
        try:
            placement = cpu_placement.derive()
            # Planned before the unit exists and created only once the command
            # is known to build; see ci-hub/validate/unwrapped_git.py.
            planned = unwrapped_git.plan(
                os.environ.get("PATH", ""), unwrapped_git_directory(root, slot)
            )
            command = launch_command(
                root,
                systemd_run,
                with_proxy,
                target,
                record,
                placement=placement,
                path=planned.path,
            )
        except ValueError as error:
            emit("refused", with_sweep(str(error)))
            return 1
        git_substitution = unwrapped_git.materialize(planned)
        previous = read_state(root)
        started_at = utc_now()
        try:
            # Rebuilt from the outcome: PATH is unchanged if the directory
            # could not be created, and when it was, the submodule fetch keeps
            # PATH's original git (NETWORK_GIT_ENV).
            command = launch_command(
                root,
                systemd_run,
                with_proxy,
                target,
                record,
                placement=placement,
                path=git_substitution.path,
                network_git_path=(
                    git_substitution.resolved_git if git_substitution.applied else None
                ),
                launch_intent=launch_intent_path(root),
            )
            # The unit appends its own output to this log; stdout here is the
            # health tick's state/summary protocol and must stay two lines.
            with log.open("a", encoding="utf-8") as retained:
                print(
                    f"{utc_now()} pressure_schedule: slot={slot} "
                    f"{git_substitution.summary()}",
                    file=retained,
                )
            intent = {
                "schema_version": run_registry.SCHEMA_VERSION,
                "kind": "bench",
                "state": "launching",
                "unit": f"{UNIT}.service",
                "target": target,
                "repo": "rrnewton/hermit",
                "checkout": str(root / "worktrees" / "validate" / slot),
                "log": str(log),
                "agent": "tick-pressure-test",
                "started_at": started_at,
                "producer": run_registry.PRODUCER,
                "admission": "ci-hub validate-lock",
                "cpu_placement": placement.record(),
                "git_substitution": git_substitution.record(),
            }
            # Validated here, without a lock, so a record the unit could not
            # write refuses before any unit exists. The unit writes it
            # (adopt_launch_intent); see FETCH_TIMEOUT_SECONDS for why
            # launch() does not.
            run_registry.parse_current_record(intent)
            write_launch_intent(root, intent)
        except BaseException:
            # No unit can exist yet: do not leave this slot's directory behind.
            remove_unwrapped_git_directory(root, slot, git_substitution.record())
            raise
        try:
            started = run(
                command,
                capture_output=True,
                text=True,
                check=False,
            )
        except OSError:
            # systemd-run itself could not be started, so no unit exists.
            remove_unwrapped_git_directory(root, slot, git_substitution.record())
            launch_intent_path(root).unlink(missing_ok=True)
            raise
        if started.returncode != 0:
            detail = (started.stderr or started.stdout).strip() or "no output"
            # systemd refused the unit, so nothing will run in this slot and
            # nothing will adopt the intent; it stays as the refusal's record
            # (the run record is a locked write; see FETCH_TIMEOUT_SECONDS).
            cleanup_error = remove_unwrapped_git_directory(
                root, slot, git_substitution.record()
            )
            write_launch_intent(
                root,
                {
                    **intent,
                    "state": "refused",
                    "result": "systemd-launch-refused",
                    "exit_code": started.returncode,
                    "detail": detail,
                    "finished_at": utc_now(),
                    **({"cleanup_error": cleanup_error} if cleanup_error else {}),
                },
            )
            emit(
                "refused",
                with_sweep(
                    f"recurring pressure test was not launched: {detail}"
                    + (f" cleanup_error={cleanup_error}" if cleanup_error else "")
                ),
            )
            return 1

        prior = ""
        if previous and previous.get("state") not in (None, "passed"):
            prior = (
                f"; previous state={previous.get('state')} "
                f"target={previous.get('target', '-')}"
            )
        emit(
            "launched",
            with_sweep(
                f"recurring pressure test launched as {UNIT}; {target_text}; "
                f"output appended to {log}{prior}"
            ),
        )
        # A directory the sweep had to leave needs a person, so the health
        # tick reports it the same way it reports a failed previous sample.
        return 1 if prior or sweep_error else 0


def wrkslots_failure_detail(completed: subprocess.CompletedProcess) -> str:
    output = completed.stderr.strip() or completed.stdout.strip()
    if not output:
        return f"exit {completed.returncode}"
    refusals = [line for line in output.splitlines() if line.startswith("REFUSED:")]
    if refusals and not output.startswith("REFUSED:"):
        # wrkslots prints unrelated registry WARNING/INCONSISTENT findings
        # before its refusal; lead with the refusal so the one-line summary
        # names the actual cause.  The full output follows unchanged.
        return "\n".join([*refusals, output])
    return output


def remove_worktree(root: Path, slot: str, machine: str) -> str | None:
    try:
        completed = subprocess.run(
            [
                str(SOURCE_ROOT / "ci-hub/bin/wrkslots"),
                # Executable authority is the immutable/current tool checkout;
                # --project-root below keeps all mutable worktree state in root.
                "--project-root",
                str(root),
                "--machine",
                machine,
                "--wait-lock",
                str(SLOT_LOCK_WAIT_SECONDS),
                "--allow-existing-unregistered-worktrees",
                "remove",
                slot,
                "--validate-complete",
                "--coordinator-pid",
                str(os.getpid()),
                "--expected-generation",
                "1",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        return f"could not launch wrkslots remove: {error}"
    if completed.returncode == 0:
        return None
    return wrkslots_failure_detail(completed)


def run_campaign(target: str, record: Path, root: Path = STATE_ROOT) -> int:
    # Bind creation and retirement to one identity.  Letting wrkslots derive
    # the machine independently at either boundary can strand the slot when
    # hostname presentation changes between a short name and an FQDN.
    machine = socket.gethostname().split(".", 1)[0]
    durable = run_registry.read_record(record)
    raw_checkout = durable.get("checkout")
    if not isinstance(raw_checkout, str):
        # No slot and no worktree, but launch() may have created the git
        # directory the record names.
        git_cleanup_error = remove_recorded_unwrapped_git_directory(
            root, durable.get("git_substitution")
        )
        raise RuntimeError(
            "pressure-test run record lacks its wrkslots checkout; "
            "state: REFUSED -- no worktree was created and no pressure test ran. "
            "remedy: relaunch the recurring pressure test to write a complete run record"
            + (f" cleanup_error={git_cleanup_error}" if git_cleanup_error else "")
        )
    checkout = Path(raw_checkout)
    slot = checkout.name
    if checkout.exists():
        git_cleanup_error = remove_unwrapped_git_directory(
            root, slot, durable.get("git_substitution")
        )
        raise RuntimeError(
            f"refusing existing pressure-test checkout {checkout}"
            + (f" cleanup_error={git_cleanup_error}" if git_cleanup_error else "")
        )

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    results = root / "ignored" / "compat-envelope" / f"pressure-{target[:12]}-{stamp}"

    state = {
        "schema": 1,
        "state": "running",
        "started_at": utc_now(),
        "target": target,
        "results": str(results),
    }
    write_state(root, state)
    added = subprocess.run(
        [
            str(SOURCE_ROOT / "ci-hub/bin/wrkslots"),
            "--project-root",
            str(root),
            "--machine",
            machine,
            "--wait-lock",
            str(SLOT_LOCK_WAIT_SECONDS),
            "--allow-existing-unregistered-worktrees",
            "create",
            slot,
            "--slot-type",
            "validate",
            "--coordinator-authorized",
            "--agent",
            "tick-pressure-test",
            "--task",
            "recurring-pressure-test",
            "--purpose",
            "run the scheduled compatibility pressure measurement",
            "--owner-pid",
            str(os.getpid()),
            "--coordinator-pid",
            str(os.getpid()),
            "--repo",
            "checkout=hermit",
            "--start",
            f"checkout={target}",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if added.returncode != 0:
        detail = wrkslots_failure_detail(added)
        state.update(
            state="failed", finished_at=utc_now(), detail=f"wrkslots create: {detail}"
        )
        git_cleanup_error = remove_unwrapped_git_directory(
            root, slot, durable.get("git_substitution")
        )
        if git_cleanup_error:
            state["cleanup_error"] = git_cleanup_error
        write_state(root, state)
        raise RuntimeError(state["detail"])

    cleanup_error = None
    try:
        submodules = subprocess.run(
            [
                # This function's only git call that uses the network; see
                # NETWORK_GIT_ENV.
                network_git(),
                "-C",
                str(checkout),
                "submodule",
                "update",
                "--init",
                "--recursive",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if submodules.returncode != 0:
            detail = submodules.stderr.strip() or submodules.stdout.strip()
            raise RuntimeError(
                f"cannot materialize Hermit submodules: {detail or submodules.returncode}"
            )
        campaign_started = sweep_now()
        outcomes = {}
        # Each part is its own validate-lock admission, and an admission binds
        # its process identity and result into the run record without
        # permitting a replacement. Reusing the bound record refused every part
        # after the first ("record already carries a different process
        # identity"; the first night, 2026-10-19, ran only red). So every part
        # after the first starts from the record as the unit wrote it.
        unbound_record = run_registry.read_record(record)
        admitted_once = False
        for part in night_parts(campaign_started):
            if part_too_late(campaign_started, sweep_now()):
                outcomes[part] = "skipped-late"
                append_sweep(
                    root,
                    {
                        "schema": 1,
                        "night": campaign_started.astimezone(SWEEP_ZONE).date().isoformat(),
                        "part": part,
                        "results": str(part_results(results, part)),
                        "result": "skipped-late",
                        "finished_at": utc_now(),
                    },
                )
                continue
            if part_is_empty(checkout, part, root):
                # Nothing to run needs no admission: each one queued behind
                # the box and its 90 s owner-watch yield for nothing (four
                # empty survey parts took 23-96 s each on 2026-10-25).
                outcomes[part] = 0
                append_sweep(
                    root,
                    {
                        "schema": 1,
                        "night": campaign_started.astimezone(SWEEP_ZONE).date().isoformat(),
                        "part": part,
                        "results": str(part_results(results, part)),
                        "result": "empty",
                        "cells": 0,
                        "finished_at": utc_now(),
                    },
                )
                continue
            if admitted_once:
                run_registry.write_current_record(record, unbound_record)
            admitted_once = True
            sweep_lines = sweep_line_count(root)
            completed = subprocess.run(
                locked_command(
                    root, checkout, target, part_results(results, part), record, part
                ),
                check=False,
            )
            outcomes[part] = completed.returncode
            if completed.returncode != 0 and sweep_line_count(root) == sweep_lines:
                # The part's own process appends its line; one refused before
                # its child ran left none, and the morning report must still
                # name it.
                append_sweep(
                    root,
                    {
                        "schema": 1,
                        "night": campaign_started.astimezone(SWEEP_ZONE).date().isoformat(),
                        "part": part,
                        "results": str(part_results(results, part)),
                        "result": "not-admitted",
                        "admission_exit": completed.returncode,
                        "finished_at": utc_now(),
                    },
                )
        failed = {part: outcome for part, outcome in outcomes.items() if outcome != 0}
        exit_code = next(
            (outcome for outcome in failed.values() if isinstance(outcome, int)), 1
        )
        state.update(
            state="passed" if not failed else "failed",
            finished_at=utc_now(),
            exit_code=0 if not failed else exit_code,
            parts=outcomes,
        )
        if failed:
            state["detail"] = "parts not passed: " + ", ".join(
                f"{part}={outcome}" for part, outcome in failed.items()
            )
    except Exception as error:
        state.update(state="failed", finished_at=utc_now(), detail=str(error))
    finally:
        cleanup_errors = [
            error
            for error in (
                remove_reusable_checkouts(checkout),
                remove_worktree(root, slot, machine),
                remove_unwrapped_git_directory(
                    root, slot, durable.get("git_substitution")
                ),
            )
            if error
        ]
        cleanup_error = "; ".join(cleanup_errors) or None
        if cleanup_error:
            state["cleanup_error"] = cleanup_error
            state["state"] = "failed"
            if state.get("exit_code") in (None, 0):
                state["exit_code"] = 1
        write_state(root, state)
        run_registry.update_record(
            record,
            state="completed" if state["state"] == "passed" else "failed",
            result=state["state"],
            exit_code=state.get("exit_code", 1),
            detail=state.get("detail"),
            finished_at=state["finished_at"],
            results=str(results),
            cleanup_error=cleanup_error,
        )

    emit(
        state["state"],
        f"target={target} results={results}"
        + (f" detail={state['detail']}" if state.get("detail") else "")
        + (f" cleanup_error={cleanup_error}" if cleanup_error else ""),
    )
    return 0 if state["state"] == "passed" else 1


def pressure_rust_script_cache(root: Path) -> Path:
    return root / "ignored" / "ci-hub" / PRESSURE_RUST_SCRIPT_CACHE


def allocated_bytes(root: Path) -> int:
    """Bytes allocated to everything under `root`, without following symlinks."""

    total = 0
    for directory, names, files in os.walk(root):
        for name in names + files:
            try:
                total += os.lstat(os.path.join(directory, name)).st_blocks * 512
            except FileNotFoundError:
                pass
    return total


def remove_cache(root: Path) -> None:
    """Remove a cache, including read-only trees tools left in it.

    Only real directories inside the cache are made writable. A symlink is
    removed as an entry, and whatever it points at is never changed.
    """

    if root.is_symlink():
        root.unlink()
        return
    if not root.exists():
        return
    os.chmod(root, 0o700)
    for directory, names, _files in os.walk(root):
        for name in names:
            path = os.path.join(directory, name)
            if not os.path.islink(path):
                os.chmod(path, 0o700)
    shutil.rmtree(root)


def bound_rust_script_cache(
    cache: Path, limit: int = PRESSURE_RUST_SCRIPT_CACHE_MAX_BYTES
) -> None:
    if cache.is_symlink() or (cache.exists() and not cache.is_dir()):
        remove_cache(cache)
    elif cache.is_dir():
        size = allocated_bytes(cache)
        if size > limit:
            print(
                f"pressure_schedule: removing the sample's rust-script cache {cache}: "
                f"{size} bytes exceeds its {limit}-byte bound",
                file=sys.stderr,
            )
            remove_cache(cache)
    cache.mkdir(parents=True, exist_ok=True)


# THE SERIES IS EMITTED ONCE. pressure-test.rs's own run emits its cells to the
# series when DEV_HERMIT_PARENT is set, which run_locked always sets, and each
# part then ran `pressure-test.rs emit-series` again. That second emit appended
# nothing but repeated the published-rows lookup: 18 lookups took 2,206 s on the
# night of 2026-10-26, eight of them full scans of about 250 s, two of those in
# second emits. A run whose own emit appended every cell result leaves
# SERIES_EMITTED_MARKER in its results (Hermit pressure-test.rs
# record_series_emitted); then the second emit is skipped, and otherwise it runs
# as before and its status decides, so a failed or older run's emit is still
# repeated and judged.
SERIES_EMITTED_MARKER = "series-emitted.json"


def emit_unless_emitted(
    emit_command: Sequence[str], results: Path, checkout: Path, environment: Mapping[str, str]
) -> tuple[subprocess.CompletedProcess | None, str | None]:
    """The part's series emit: (its status, "run" or "post-run"), or (None, None)
    when the run produced no run.json."""

    if not (results / "run.json").is_file():
        return None, None
    if (results / SERIES_EMITTED_MARKER).is_file():
        return subprocess.CompletedProcess(list(emit_command), 0), "run"
    return subprocess.run(emit_command, cwd=checkout, env=environment, check=False), "post-run"


def run_sample(checkout: Path, results: Path, environment: Mapping[str, str]) -> int:
    """The parity sample: its exit status is the run's, as before the sweep."""

    run_command, emit_command = sample_commands(checkout, results)
    run_command = with_reuse(run_command, checkout)
    ran = subprocess.run(run_command, cwd=checkout, env=environment, check=False)
    emitted, _ = emit_unless_emitted(emit_command, results, checkout, environment)
    if ran.returncode != 0:
        return ran.returncode
    if emitted is None:
        print(
            "pressure_schedule: run produced no run.json; series was not emitted",
            file=sys.stderr,
        )
        return 2
    return emitted.returncode


def run_locked(
    checkout: Path,
    results: Path,
    root: Path = STATE_ROOT,
    measure: Callable[[], tuple[float, int]] = idle_cores,
    part: str = "red",
    validation: Mapping[str, object] | None = None,
    size: Callable[..., int] | None = None,
) -> int:
    run_timeout = part_run_timeout(part)
    environment = dict(os.environ)
    environment["DEV_HERMIT_PARENT"] = str(root)
    cache = pressure_rust_script_cache(root)
    bound_rust_script_cache(cache)
    environment["XDG_CACHE_HOME"] = str(cache)
    started_at = utc_now()
    sweep = {
        "schema": 1,
        "night": sweep_now().astimezone(SWEEP_ZONE).date().isoformat(),
        "part": part,
        "results": str(results),
        "started_at": started_at,
        "run_timeout_seconds": run_timeout,
    }
    if validation is not None:
        sweep["target_validation"] = dict(validation)
    if part == "sample":
        code = run_sample(checkout, results, environment)
        append_sweep(root, {**sweep, "result": "sample", "run_exit": code, "finished_at": utc_now()})
        return code
    if part in ARCHIVED_LANES:
        return run_archived_lane(checkout, results, part, sweep, root, environment)
    backend = part_backend(part)
    resurvey = part.startswith("resurvey-")
    repetitions = 1 if resurvey else SWEEP_REPETITIONS
    cells_file = None
    if part == "red":
        cells, excluded = red_cells(checkout)
        sweep["cells"] = len(cells)
        sweep["excluded"] = {
            "reason": NATIVE_NAKED_EXCLUSION,
            "count": len(excluded),
            "cells": [
                f"{c['lane']}/{c['category']}/{c['test']}/{c['mode']}@{c['backend']}"
                for c in excluded
            ],
        }
        if not cells:
            print("pressure_schedule: red: no red cell to run", file=sys.stderr)
            append_sweep(root, {**sweep, "result": "empty", "finished_at": utc_now()})
            return 0
        cells_file = results.with_name(f"{results.name}.cells.jsonl")
        write_cells_file(cells_file, cells)
        sweep["cells_file"] = str(cells_file)
    elif part == PARITY_ALL_PART and not parity_all_supported(checkout):
        print(
            "pressure_schedule: parity-all: this Hermit's pressure-test.rs has no --parity-all; "
            "nothing to run",
            file=sys.stderr,
        )
        append_sweep(root, {**sweep, "result": "unsupported", "finished_at": utc_now()})
        return 0
    elif backend is not None and not resurvey:
        cells = survey_cells(checkout, backend, root)
        sweep["cells"] = len(cells)
        if not cells:
            print(
                f"pressure_schedule: {part}: {backend} has no survey-failed or promoted cell; nothing to run",
                file=sys.stderr,
            )
            append_sweep(root, {**sweep, "result": "empty", "finished_at": utc_now()})
            return 0
        cells_file = results.with_name(f"{results.name}.cells.jsonl")
        write_cells_file(cells_file, cells)
        sweep["cells_file"] = str(cells_file)
        run_timeout = survey_run_timeout(len(cells))
        sweep["run_timeout_seconds"] = run_timeout
    idle, cpus = measure()
    guests = sweep_guests(idle)
    sweep.update(
        idle_cores=round(idle, 1),
        cpus=cpus,
        guests=guests,
        repetitions=repetitions,
    )
    if guests < SWEEP_MIN_GUESTS:
        print(
            f"pressure_schedule: {part} refused: {idle:.1f} of {cpus} cores were idle, "
            f"so half of them allows {guests} guests, fewer than the "
            f"{SWEEP_MIN_GUESTS} the population's declared occupancy needs "
            f"within {run_timeout} s",
            file=sys.stderr,
        )
        append_sweep(root, {**sweep, "result": "refused-width", "finished_at": utc_now()})
        return 3
    print(
        f"pressure_schedule: {part} width {guests} guests ({idle:.1f} of {cpus} cores idle)",
        file=sys.stderr,
    )
    if part == PARITY_ALL_PART:
        repetitions = 1
        sweep["repetitions"] = repetitions
        run_command, emit_command = parity_all_commands(checkout, results, guests, run_timeout)
    elif backend is None:
        run_command, emit_command = pressure_commands(checkout, results, guests, cells_file)
    else:
        run_command, emit_command = survey_commands(
            checkout, results, backend, cells_file, guests, run_timeout, repetitions
        )
    run_command = with_reuse(run_command, checkout)
    safe = (size or memory_safe_guests)(run_command, guests, results, checkout, environment)
    if safe < guests:
        print(
            f"pressure_schedule: {part} width {safe} guests: the highest memory-safe "
            f"manifest-guest cap for this population (CPU headroom allowed {guests})",
            file=sys.stderr,
        )
        guests = safe
        sweep["guests"] = guests
        sweep["memory_safe_guests"] = safe
        run_command = with_width(run_command, guests)
    ran = subprocess.run(run_command, cwd=checkout, env=environment, check=False)
    emitted, emitted_by = emit_unless_emitted(emit_command, results, checkout, environment)
    reason = unjudged(results)
    if reason is None and resurvey:
        sweep["promoted"] = promote(root, checkout, results, sweep["night"])
    append_sweep(
        root,
        {
            **sweep,
            "result": "ran" if reason is None else "unjudged",
            "run_exit": ran.returncode,
            "emit_exit": None if emitted is None else emitted.returncode,
            **({"emitted_by": emitted_by} if emitted_by else {}),
            "finished_at": utc_now(),
            **({"detail": reason} if reason else {}),
        },
    )
    if reason is not None:
        print(f"pressure_schedule: {part} did not complete: {reason}", file=sys.stderr)
        return ran.returncode or 2
    if ran.returncode != 0:
        print(
            f"pressure_schedule: {part} judged every cell; pressure-test exit "
            f"{ran.returncode} reports cells short of CLEAN, as these cells are",
            file=sys.stderr,
        )
    if emitted is None:
        print(
            "pressure_schedule: run produced no run.json; series was not emitted",
            file=sys.stderr,
        )
        return 2
    return emitted.returncode


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    if argv[:1] == ["_run"]:
        worker = argparse.ArgumentParser(add_help=False)
        worker.set_defaults(command="_run")
        worker.add_argument("_command")
        worker.add_argument("--target", required=True)
        worker.add_argument("--record", type=Path, required=True)
        worker.add_argument("--state-root", type=Path, default=STATE_ROOT)
        worker.add_argument("--launch-intent", type=Path)
        return worker.parse_args(argv)
    if argv[:1] == ["_locked"]:
        locked = argparse.ArgumentParser(add_help=False)
        locked.set_defaults(command="_locked")
        locked.add_argument("_command")
        locked.add_argument("--checkout", type=Path, required=True)
        locked.add_argument("--results", type=Path, required=True)
        locked.add_argument("--state-root", type=Path, default=STATE_ROOT)
        locked.add_argument(
            "--part", choices=(*SWEEP_PARTS, *RESURVEY_PARTS), default="red"
        )
        return locked.parse_args(argv)
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    launch_parser = subparsers.add_parser(
        "launch", help="launch tonight's detached sweep when it is due"
    )
    launch_parser.add_argument("--state-root", type=Path, default=STATE_ROOT)
    launch_parser.add_argument(
        "--now",
        action="store_true",
        help="launch outside the nightly window or after tonight's sweep started",
    )
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.command == "launch":
        if not args.now:
            due, reason = sweep_due(args.state_root)
            if not due:
                # Exit 0: tick-hub reports this gate only on failure.
                emit("not-due", reason)
                return 0
        return launch(args.state_root)
    if args.command == "_run":
        if args.launch_intent is not None:
            try:
                adopt_launch_intent(
                    args.launch_intent, args.record, args.target, args.state_root
                )
            except RuntimeError as error:
                # The run record still names the previous run; leave it.
                emit("failed", str(error))
                return 1
        try:
            return run_campaign(args.target, args.record, args.state_root)
        except RuntimeError as error:
            try:
                run_registry.update_record(
                    args.record,
                    state="failed",
                    result="failed",
                    exit_code=1,
                    detail=str(error),
                    finished_at=utc_now(),
                )
            except RuntimeError as record_error:
                emit(
                    "failed",
                    f"{error}; run record update also failed: {record_error}",
                )
                return 1
            emit("failed", str(error))
            return 1
    if args.command == "_locked":
        return run_locked(
            args.checkout,
            args.results,
            args.state_root,
            part=args.part,
            validation=target_validation(args.state_root, args.checkout),
        )
    raise AssertionError(args.command)


if __name__ == "__main__":
    raise SystemExit(main())

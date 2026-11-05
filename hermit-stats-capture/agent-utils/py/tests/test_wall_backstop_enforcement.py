"""A DERIVED wall backstop must reach the step that runs, not merely the pure function.

``resolved_wall_timeout`` was pinned by unit tests and by the pre-flight ``--run-timeout``
ordering check, and by nothing else. Replacing the call at the ENFORCEMENT site in
``scheduler.py`` with the pre-derivation fallback (``step.timeout``, else the document default,
else 1800) left every suite in both engines green: a step declaring only a CPU budget went on
running under 1800 s while the docs and the manifest said otherwise. ``make cross`` cannot see
that either, because it is a py-vs-rs differential and the defect is symmetric.

This is the Python half of ``rs/dagrun/tests/wall_backstop_enforcement.rs``, and it
pins the chain in two links, because a derived ceiling is floored at ``DEFAULT_STEP_TIMEOUT`` and
so cannot be waited out inside a test:

1. a real run of a step that declares only ``cpu_timeout`` journals the DERIVED ceiling as the
   bound it ran under; and
2. the number journalled under that name is the number the wall killer actually enforces — a step
   given a 2-second ceiling is reaped at 2 seconds and reports 2.

Both legs run unboxed, so neither can self-skip: the wall bound is a scheduler ``wait`` with a
deadline and is in force on the uncontained lane, which ``capabilities`` already says out loud.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

from dagrun import DagConfig, Runner, Step
from dagrun.attribution import LOG_DIR_ENV
from dagrun.cgroup import NoopCgroups


def _run(log_dir: Path, step: Step) -> list[dict[str, str]]:
    previous = os.environ.get(LOG_DIR_ENV)
    os.environ[LOG_DIR_ENV] = str(log_dir)
    try:
        Runner(
            DagConfig(steps=(step,)), max_steps=1, max_cpus=1, cgroups=NoopCgroups()
        ).run()
    finally:
        if previous is None:
            os.environ.pop(LOG_DIR_ENV, None)
        else:
            os.environ[LOG_DIR_ENV] = previous
    return [
        json.loads(line)
        for line in (log_dir / "journal.jsonl").read_text().splitlines()
        if line.strip()
    ]


def _one(records: list[dict[str, str]], event: str) -> dict[str, str]:
    matches = [r for r in records if r.get("event") == event]
    assert len(matches) == 1, f"expected exactly one {event} record, got {matches}"
    return matches[0]


def test_a_step_declaring_only_a_cpu_budget_runs_under_the_derived_ceiling(
    tmp_path: Path,
) -> None:
    # 900 declared CPU-seconds derive 3 * 900 = 2700, above the 1800 floor, so the derivation is
    # what governs. The command exits immediately: this leg is about the bound the step RAN
    # UNDER, not about breaching it.
    step = Step("g", "derived", "declares only a CPU budget", "true", cpu_timeout=900)
    end = _one(_run(tmp_path / "derived", step), "step_end")
    # 2700, named literally. 1800 here is the pre-derivation fallback still being enforced while
    # the derivation sits unused one call away; 900 or 300 would be an unscaled or unfactored
    # budget reaching the runner.
    assert end["wall_limit_s"] == "2700", (
        f"the ceiling the step actually ran under must be the DERIVED one: {end}"
    )


def test_the_journalled_wall_ceiling_is_the_one_the_killer_enforces(tmp_path: Path) -> None:
    # The second link. Without it, the leg above only shows that a number was written down; this
    # shows that the number written down under `wall_limit_s` is the deadline the step is reaped
    # on.
    step = Step("g", "hang", "outlives its declared ceiling", "sleep 30", timeout=2)
    started = time.time()
    records = _run(tmp_path / "declared", step)
    elapsed = time.time() - started

    breach = _one(records, "step_timeout")
    assert breach["limit_s"] == "2"
    assert breach["unit"] == "wall_seconds"
    end = _one(records, "step_end")
    assert end["wall_limit_s"] == breach["limit_s"], (
        f"the journalled ceiling and the enforced deadline must be one number: {end} {breach}"
    )
    assert elapsed < 25.0, (
        "the step was reaped on its 2-second ceiling, not left to finish its 30-second sleep; "
        f"the run took {elapsed}s"
    )

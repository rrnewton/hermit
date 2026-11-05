"""Tests for the configurable inner-parallelism (concurrency) flag appended to step commands."""

from __future__ import annotations

from dagrun import (
    DagConfig,
    ResourceHint,
    Step,
    command_with_inner_jobs,
    dag_from_json,
    dag_to_json,
    render_jobs_flag,
    run_dag,
)


def test_render_jobs_flag_forms() -> None:
    # %d -> substitute (no auto-space); trailing '=' -> concatenate; otherwise space-separated.
    assert render_jobs_flag("-j", 4) == "-j 4"
    assert render_jobs_flag("-j%d", 4) == "-j4"
    assert render_jobs_flag("--jobs=", 8) == "--jobs=8"
    assert render_jobs_flag("--num-threads", 2) == "--num-threads 2"
    assert render_jobs_flag("--threads=%d", 3) == "--threads=3"


def test_command_with_inner_jobs() -> None:
    s = Step("g", "j", "", "make")
    assert command_with_inner_jobs(s, "-j", None) == "make"  # no inner jobs -> unchanged
    assert command_with_inner_jobs(s, "-j", 4) == "make -j 4"  # default template
    s2 = Step("g", "j", "", "cargo build", jobs_flag="-j%d")
    assert command_with_inner_jobs(s2, "-j", 8) == "cargo build -j8"  # step override
    s3 = Step("g", "j", "", "mytool", jobs_flag="")
    assert command_with_inner_jobs(s3, "-j", 4) == "mytool"  # empty template disables append
    s4 = Step("g", "j", "", "fixed", jobs_flag="   ")
    assert command_with_inner_jobs(s4, "-j", 4) == "fixed"  # whitespace is also empty


def test_jobs_flag_json_roundtrip() -> None:
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "j",
                "",
                "make",
                hint=ResourceHint(preferred_inner_jobs=4),
                jobs_flag="-j%d",
            ),
        ),
        default_jobs_flag="--jobs=",
    )
    back = dag_from_json(dag_to_json(cfg))
    assert back.default_jobs_flag == "--jobs="
    assert back.steps[0].jobs_flag == "-j%d"
    assert dag_to_json(back) == dag_to_json(cfg)  # canonical JSON is a fixed point


def test_jobs_flag_appended_at_runtime() -> None:
    # The command passes only when it receives exactly the expected appended token.
    good = DagConfig(
        steps=(
            Step(
                "g",
                "j",
                "",
                'c() { [ "$*" = "-j4" ]; }; c',
                hint=ResourceHint(preferred_inner_jobs=4),
                jobs_flag="-j%d",
            ),
        )
    )
    assert run_dag(good, jobs=4, verbosity=0).ok

    # A wrong expectation fails, proving the append actually happened (not a no-op).
    bad = DagConfig(
        steps=(
            Step(
                "g",
                "j",
                "",
                'c() { [ "$*" = "-jWRONG" ]; }; c',
                hint=ResourceHint(preferred_inner_jobs=4),
                jobs_flag="-j%d",
            ),
        )
    )
    assert not run_dag(bad, jobs=4, verbosity=0).ok

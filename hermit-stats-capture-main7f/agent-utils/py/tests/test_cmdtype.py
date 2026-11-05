"""Known cmdtype command-line shapes and DAGRUN_EXTRA_ARGS delivery."""

from __future__ import annotations

import json

import pytest

from dagrun import (
    CmdType,
    DagConfig,
    DagJsonError,
    ResourceHint,
    Step,
    cmdtype_extra_args,
    command_with_inner_jobs,
    dag_from_json,
    dag_to_json,
    run_dag,
)


def _step(cmdtype: CmdType, cmd: str, *, jobs_flag: str | None = None) -> Step:
    return Step(
        "g",
        "j",
        "",
        cmd,
        hint=ResourceHint(preferred_inner_jobs=3),
        jobs_flag=jobs_flag,
        cmdtype=cmdtype,
    )


def test_every_cmdtype_has_a_stable_value_and_known_arguments() -> None:
    expected = {
        CmdType.UNKNOWN: None,
        CmdType.MAKE: "-j3",
        CmdType.CARGO_BUILD: "--jobs 3",
        CmdType.CARGO_TEST: "--jobs 3",
        CmdType.CARGO_NEXTEST: "--test-threads 3",
        CmdType.GENERIC_DASH_J_COMMAND: "-j3",
        CmdType.GENERIC_WITH_FLAG: "--workers 3",
    }
    for cmdtype, arguments in expected.items():
        step = _step(
            cmdtype,
            "true",
            jobs_flag="--workers" if cmdtype is CmdType.GENERIC_WITH_FLAG else None,
        )
        assert cmdtype_extra_args(step, 3) == arguments


def test_known_simple_command_gets_arguments_appended() -> None:
    step = _step(
        CmdType.CARGO_BUILD,
        "sh -c 'test \"$#\" -eq 2 && test \"$1\" = --jobs && test \"$2\" = 3' capture",
    )
    assert command_with_inner_jobs(step, "-j", 3).endswith("capture --jobs 3")
    assert run_dag(DagConfig(steps=(step,)), jobs=3, verbosity=0).ok


def test_compound_command_expands_extra_args_once() -> None:
    step = _step(
        CmdType.CARGO_BUILD,
        'capture() { [ "$#" -eq 2 ] && [ "$1" = --jobs ] && [ "$2" = 3 ]; }; '
        'final() { [ "$#" -eq 0 ]; }; capture $DAGRUN_EXTRA_ARGS && final',
    )
    step.env["DAGRUN_EXTRA_ARGS"] = "poison"
    assert command_with_inner_jobs(step, "-j", 3) == step.cmd
    assert run_dag(DagConfig(steps=(step,)), jobs=3, verbosity=0).ok


def test_unknown_neither_appends_cmdtype_arguments_nor_sets_the_variable() -> None:
    step = _step(
        CmdType.UNKNOWN,
        'test -z "${DAGRUN_EXTRA_ARGS+x}"',
        jobs_flag="",
    )
    step.env["DAGRUN_EXTRA_ARGS"] = "poison"
    assert command_with_inner_jobs(step, "-j", 3) == step.cmd
    assert run_dag(DagConfig(steps=(step,)), jobs=3, verbosity=0).ok


def test_multi_word_extra_args_must_not_be_quoted() -> None:
    document = {
        "steps": [
            {
                "group": "g",
                "job": "j",
                "cmd": 'cargo build "$DAGRUN_EXTRA_ARGS"',
                "cmdtype": "cargo-build",
                "hint": {"preferred_inner_jobs": 3},
            }
        ]
    }
    with pytest.raises(DagJsonError, match="must be unquoted.*multiple shell words"):
        dag_from_json(json.dumps(document))


def test_compound_command_without_extra_args_is_refused() -> None:
    document = {
        "steps": [
            {
                "group": "g",
                "job": "j",
                "cmd": "prepare && cargo build",
                "cmdtype": "cargo-build",
                "hint": {"preferred_inner_jobs": 3},
            }
        ]
    }
    with pytest.raises(DagJsonError, match="compound cmd.*must place unquoted"):
        dag_from_json(json.dumps(document))


@pytest.mark.parametrize(
    ("cmdtype", "jobs_flag", "message"),
    [
        ("generic-with-flag", None, "requires a non-empty jobs_flag"),
        ("cargo-build", "--workers", "jobs_flag is valid with cmdtype generic-with-flag"),
    ],
)
def test_cmdtype_and_jobs_flag_combinations_are_unambiguous(
    cmdtype: str, jobs_flag: str | None, message: str
) -> None:
    step: dict[str, object] = {
        "group": "g",
        "job": "j",
        "cmd": "true",
        "cmdtype": cmdtype,
    }
    if jobs_flag is not None:
        step["jobs_flag"] = jobs_flag
    with pytest.raises(DagJsonError, match=message):
        dag_from_json(json.dumps({"steps": [step]}))


def test_cmdtype_round_trip_and_unknown_value_refusal() -> None:
    cfg = DagConfig(steps=(_step(CmdType.MAKE, "make"),))
    encoded = dag_to_json(cfg)
    assert dag_from_json(encoded).steps[0].cmdtype is CmdType.MAKE
    assert dag_to_json(dag_from_json(encoded)) == encoded

    bad = json.dumps(
        {"steps": [{"group": "g", "job": "j", "cmd": "true", "cmdtype": "cargo"}]}
    )
    with pytest.raises(DagJsonError, match="valid values: unknown, make, cargo-build"):
        dag_from_json(bad)

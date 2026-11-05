"""A rebuilt DagConfig must be provably the same configuration, field by field.

``DagConfig(steps=steps)`` — the natural thing to write once a loader hands out steps and keeps
its configuration elsewhere — reverts every top-level field the call does not name.  Nothing
reports it: the reverted fields appear in no diff, no warning, and no failure.  In the incident
that produced this test file, a lane's 600 s wall budget became the 1800 s module default and a
privileged lane's 120 s became 1800 s, a 3x and 15x loosening that produced *no symptom at all*.
The scarce-resource cap dropped the same way and merely happened to fail loudly, by hanging.

So the check cannot be "does it hang".  It is a CARRY ASSERTION: compare the rebuilt config with
the one it came from and name every field that differs.  Both directions are asserted here — a
comparison that reported everything would pass the negative case alone.

The comparison is deliberately not dataclass equality.  ``DAG_CONFIG_FIELDS`` is a written-down
checklist and :func:`dag_config_carry_diff` refuses to run when it drifts from ``DagConfig``, so
a NEW field forces a decision instead of joining a silent default — which is exactly this bug's
shape.  The Rust twin gets the same guarantee from exhaustive destructuring, where the omission
is a compile error.
"""

from __future__ import annotations

import dataclasses

import pytest

from dagrun import (
    DAG_CONFIG_FIELDS,
    DagConfig,
    ResourceHint,
    Step,
    WriteDomainPolicy,
    dag_config_carry_diff,
)
from dagrun.io import UNCARRIED_CONFIG_KEYS, DagJsonError, dag_from_json, dag_to_json
from dagrun.model import _check_dag_config_fields


def configured() -> DagConfig:
    """A config with every top-level field DELIBERATELY off its default.

    ``mem_cap_factor`` 1.25 and ``outer_mem_safety_factor`` 1.0 equalled their defaults in the
    live incident, so dropping them was harmless *by coincidence*; a fixture that reused the
    defaults would inherit that coincidence and stop testing anything.
    """
    return DagConfig(
        steps=(
            Step(
                group="g",
                job="j",
                desc="d",
                cmd="true",
                hint=ResourceHint(),
                write_domains=["shared-cargo-target"],
            ),
        ),
        description="a real lane",
        resource_caps={"widget_guest": 1, "manifest_guest": 4},
        mem_cap_factor=1.5,
        mem_cap_floor_bytes=4 * 1024**3,
        outer_mem_safety_factor=1.2,
        default_step_timeout=600,
        default_jobs_flag="--jobs {n}",
        default_jobs_env="CARGO_BUILD_JOBS",
        default_step_mem_cap_bytes=None,
        default_step_cpu_count=4,
        default_step_cpu_timeout=120,
        known_failures=frozenset({"g.j"}),
        cpu_timeout_multiplier=2.0,
        cpu_timeout_platform="github-hosted",
        write_domain_policy=WriteDomainPolicy(
            require_explicit=True, allowed_domains=frozenset({"shared-cargo-target"})
        ),
    )


def test_carry_diff_is_empty_for_a_config_compared_with_itself() -> None:
    cfg = configured()
    assert dag_config_carry_diff(cfg, cfg) == []
    assert dag_config_carry_diff(DagConfig(steps=()), DagConfig(steps=())) == []


def test_carry_diff_names_every_field_a_default_rebuild_drops() -> None:
    cfg = configured()
    # The exact footgun from the field: keep the steps, revert everything else.
    rebuilt = DagConfig(steps=cfg.steps)
    diff = dag_config_carry_diff(cfg, rebuilt)
    named = [line.split(":")[0] for line in diff]
    # Every field EXCEPT `steps` -- the one the call named, and the only one that survived.
    assert named == [f for f in DAG_CONFIG_FIELDS if f != "steps"], diff
    # The loudest one in the live incident, spelled out. The rebuilt default is now the 0
    # "derive it" sentinel rather than a materialized 1800, so a dropped 600 lands there; the
    # effective bound a step would then run under is still resolved, not zero.
    assert "default_step_timeout: 600 -> 0" in diff


def test_with_steps_carries_every_field_the_default_rebuild_drops() -> None:
    cfg = configured()
    assert dag_config_carry_diff(cfg, cfg.with_steps(cfg.steps)) == []
    # And it really does replace the steps, so it is usable where the footgun was written.
    assert dag_config_carry_diff(cfg, cfg.with_steps(())) == ["steps: g.j -> "]


# ------------------------------------- the product's own rebuild sites (#21)
#
# The carry assertion is worth nothing if the only configs it ever compares are built in a test.
# These two functions are the ENTIRE set of places the product rebuilds a DagConfig around a new
# step list, they both go through `with_steps`, and both are on the default `run` path: a plan is
# applied on every run, and `--stress N` expands the graph. Assert the policy survives them.


def test_applying_a_plan_carries_the_whole_lane_policy_forward() -> None:
    from dagrun.estimates import apply_plan_to_config, build_plan

    base = configured()
    cfg = dataclasses.replace(
        base,
        steps=(dataclasses.replace(base.steps[0], hint=ResourceHint(est_duration_s=3.0)),),
    )
    applied = apply_plan_to_config(cfg, build_plan(cfg, {}))
    # Only the steps may differ (the plan writes their hints); every top-level field is carried.
    assert dag_config_carry_diff(cfg, applied) == []
    # ...and the plan really was applied: this is a rebuilt config carrying the plan's estimate,
    # not the argument handed back.
    assert applied is not cfg
    assert applied.steps[0].hint.est_duration_s == 3.0
    # The field that a field-by-field rebuild here actually dropped once, spelled out: the
    # run-budget clamp reads it immediately afterwards.
    assert applied.default_step_cpu_count == 4
    assert applied.default_step_timeout == 600


def test_the_stress_expansion_carries_the_whole_lane_policy_forward() -> None:
    from dagrun.cli import _expand_stress

    cfg = configured()
    expanded = _expand_stress(cfg, 3)
    diff = dag_config_carry_diff(cfg, expanded)
    named = [line.split(":")[0] for line in diff]
    # `steps` and `resource_caps` are what a stress expansion is FOR; nothing else may move.
    assert named == ["steps", "resource_caps"], diff
    assert len(expanded.steps) == 3
    assert expanded.default_step_timeout == 600
    assert expanded.cpu_timeout_multiplier == 2.0


def test_an_absent_default_cap_is_reported_as_absent_not_as_zero() -> None:
    # ABSENT IS NOT ZERO: "disable the cap" and "cap at 0" are opposite instructions, so the
    # report must not collapse them into the same line.
    absent = dataclasses.replace(DagConfig(steps=()), default_step_mem_cap_bytes=None)
    zero = dataclasses.replace(DagConfig(steps=()), default_step_mem_cap_bytes=0)
    assert dag_config_carry_diff(absent, zero) == [
        "default_step_mem_cap_bytes: <absent> -> 0"
    ]


def test_a_nan_factor_is_not_reported_as_a_dropped_field() -> None:
    # NaN != NaN, so a naive float comparison would report an untouched config as changed -- an
    # assertion that fires on a config nobody rebuilt is one nobody keeps.
    nan = dataclasses.replace(DagConfig(steps=()), mem_cap_factor=float("nan"))
    assert dag_config_carry_diff(nan, nan) == []


def test_the_field_checklist_is_exactly_the_dataclass() -> None:
    # DAG_CONFIG_FIELDS is prose until something checks it: this is the Python stand-in for the
    # Rust edition's exhaustive destructuring.
    assert _check_dag_config_fields() == []
    assert len(DAG_CONFIG_FIELDS) == len(dataclasses.fields(DagConfig))
    diff = dag_config_carry_diff(configured(), DagConfig(steps=()))
    # `steps` differs too (one step versus none), so a fully-configured config differs from the
    # defaults in EVERY field.
    assert len(diff) == len(DAG_CONFIG_FIELDS), diff


# --------------------------------------------- uncarried top-level config keys (#21)


#: The six keys the loader must refuse, WRITTEN OUT rather than read from the production
#: constant.
#:
#: Parametrising over :data:`UNCARRIED_CONFIG_KEYS` was a tautology: deleting two names from the
#: production tuple deleted the two cases that would have caught it, and the suite stayed green
#: (1821 -> 1819 passed, 0 failed) while two previously-refused keys went back to silently
#: defaulting.  A literal list is the only kind that can fail.  It is the same list the Rust
#: edition's ``io.rs`` tests write out, and the cross differential drives both binaries with each
#: of these keys — so the "byte for byte in both editions" claim in the two doc comments is now
#: something a check can break.
REFUSED_KEYS = (
    "default_step_mem_cap_bytes",
    "default_step_cpu_count",
    "default_step_cpu_timeout",
    "cpu_timeout_multiplier",
    "cpu_timeout_platform",
    "known_failures",
)


@pytest.mark.parametrize("key", REFUSED_KEYS)
def test_a_top_level_key_the_format_cannot_carry_is_refused_by_name(key: str) -> None:
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json('{"%s": 5, "steps": []}' % key)
    message = str(excinfo.value)
    assert key in message
    assert "SILENTLY replaced by a default" in message


def test_the_refused_key_set_is_exactly_the_six_names_the_contract_lists() -> None:
    # The other direction, so the literal list cannot silently GROW either: a key added to the
    # production tuple without being added to the shared contract (and to the Rust edition, and
    # to the cross differential) is a document that loads on one build and is rejected on the
    # other.
    assert UNCARRIED_CONFIG_KEYS == REFUSED_KEYS


def test_the_refusal_counts_and_names_every_offending_key_at_once() -> None:
    with pytest.raises(DagJsonError) as excinfo:
        dag_from_json(
            '{"default_step_cpu_timeout": 5, "cpu_timeout_multiplier": 2.0, "steps": []}'
        )
    message = str(excinfo.value)
    assert "2 top-level key(s)" in message
    assert "default_step_cpu_timeout, cpu_timeout_multiplier" in message


def test_a_key_naming_no_config_field_at_all_is_still_tolerated() -> None:
    # Forward compatibility: a key nobody has ever implemented cannot masquerade as a setting
    # that took effect, so it is NOT the dropped-field bug and stays accepted.
    cfg = dag_from_json('{"future_thing": 5, "steps": []}')
    assert cfg.steps == ()


def test_every_key_the_serializer_emits_survives_a_round_trip() -> None:
    # The carry assertion applied to this package's OWN loader/serializer pair: whatever
    # dag_to_json writes, dag_from_json must read back to the same configuration, or the format
    # itself is silently substituting defaults.
    doc = """{
        "description": "a real lane",
        "resource_caps": {"widget_guest": 1, "manifest_guest": 4},
        "mem_cap_factor": 1.5,
        "mem_cap_floor_bytes": 4294967296,
        "outer_mem_safety_factor": 1.2,
        "default_step_timeout": 600,
        "default_jobs_flag": "--jobs {n}",
        "write_domain_policy": {"require_explicit": true, "allowed_domains": ["shared"]},
        "steps": [{"group": "g", "job": "j", "cmd": "true", "write_domains": []}]
    }"""
    cfg = dag_from_json(doc)
    again = dag_from_json(dag_to_json(cfg))
    assert dag_config_carry_diff(cfg, again) == []
    # and the values really were non-default, so an all-defaults round trip cannot pass by
    # accident.
    assert cfg.default_step_timeout == 600
    assert len(cfg.resource_caps) == 2

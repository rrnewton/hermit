"""The MACHINE-level env channel for a step's inner width.

These tests exist because the capacity refusal must keep its teeth. A step that bakes its
width into its own command cannot be narrowed by clamping its cgroup quota -- that would leave
the original worker count running inside a smaller box, a slowdown disguised as a limit. The
env channel gives such a step a way to be narrowed honestly; it must NOT give it a way to be
waved through.
"""

from __future__ import annotations

import json
import shlex
from pathlib import Path

import pytest

from dagrun import (
    DagConfig,
    DagJsonError,
    ResourceHint,
    Runner,
    apply_plan_to_config,
    build_plan,
    cap_config_max_cpus,
    dag_from_json,
    dag_to_json,
    run_dag_limited,
)
from dagrun.cgroup import NoopCgroups
from dagrun.estimates import Planner
from dagrun.model import (
    JOBS_ENV_ENV,
    Step,
    env_with_inner_jobs,
    resolve_jobs_env,
    step_width_is_resizable,
)


def _step(
    *,
    jobs_flag: str | None = None,
    jobs_env: str | None = None,
    hint: ResourceHint | None = None,
) -> Step:
    """A step whose command no appended flag can reach, with the two channels spelled out.

    The channels are named parameters rather than ``**kw``: ``Step`` has fields of a dozen
    different types, so a forwarding ``**kw`` cannot be annotated at all without widening every
    one of them, and this file was failing ``mypy --strict`` -- and therefore ``make validate``
    -- for exactly that reason.
    """

    return Step(
        "g",
        "j",
        "",
        "cargo build --workspace && cargo build --bin x",
        jobs_flag=jobs_flag,
        jobs_env=jobs_env,
        hint=hint or ResourceHint(),
    )


def test_host_declares_the_channel_and_the_graph_does_not_have_to() -> None:
    """The motivating case: a compound command that no appended flag can reach."""
    step = _step(jobs_flag="")
    assert env_with_inner_jobs(step, "CARGO_BUILD_JOBS", 4) == {"CARGO_BUILD_JOBS": "4"}


@pytest.mark.parametrize("name", (
    "BASH_COMPAT", "BASH_ENV", "BASH_XTRACEFD", "CDPATH", "ENV",
    "EXECIGNORE", "GLOBIGNORE", "PATH", "POSIXLY_CORRECT",
))
def test_jobs_env_shell_controls_refused_at_every_configuration_boundary(name: str) -> None:
    with pytest.raises(ValueError, match="shell startup/control variable"):
        resolve_jobs_env(env={JOBS_ENV_ENV: name})
    with pytest.raises(ValueError, match="shell startup/control variable"):
        step_width_is_resizable(_step(jobs_env=name, jobs_flag="-j%d"), "-j", "")
    for doc in (
        {"default_jobs_env": name, "steps": []},
        {"steps": [{"group": "g", "job": "j", "cmd": "true", "jobs_env": name}]},
    ):
        with pytest.raises(DagJsonError, match="shell startup/control variable"):
            dag_from_json(json.dumps(doc))


@pytest.mark.parametrize("name", ("LINENO", "IFS"))
def test_jobs_env_dynamic_or_fresh_shell_reset_refuses_guest(tmp_path: Path, name: str) -> None:
    marker = tmp_path / "guest"
    step = Step("g", "j", "", f"printf ran > {shlex.quote(str(marker))}",
                jobs_flag="", hint=ResourceHint(preferred_inner_jobs=1))
    result = run_dag_limited(DagConfig(steps=(step,), default_jobs_env=name),
                             max_steps=1, max_cpus=1, cgroups=None, verbosity=0)
    assert not result.ok
    assert result.outcomes[0].returncode == 125
    assert "did not retain assigned width 1" in result.outcomes[0].summary
    assert not marker.exists(), "the guest command must not run"


def test_jobs_env_removed_before_bash_startup_then_delivered(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed = tmp_path / "startup"
    guest = tmp_path / "guest"
    hook = tmp_path / "startup.sh"
    hook.write_text(f"printf '%s\\n' \"${{WORKER_JOBS-unset}}\" >> {shlex.quote(str(observed))}\n")
    monkeypatch.setenv("WORKER_JOBS", "77")
    step = Step("g", "j", "", f"printf '%s' \"$WORKER_JOBS\" > {shlex.quote(str(guest))}",
                jobs_flag="", hint=ResourceHint(preferred_inner_jobs=2),
                env={"WORKER_JOBS": "99", "BASH_ENV": str(hook)})
    result = run_dag_limited(DagConfig(steps=(step,), default_jobs_env="WORKER_JOBS"),
                             max_steps=1, max_cpus=2, cgroups=None, verbosity=0)
    assert result.ok
    assert guest.read_text() == "2"
    assert observed.read_text().splitlines()[0] == "unset"


def test_jobs_env_startup_channel_cannot_execute_before_refusal(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    startup = tmp_path / "startup"
    guest = tmp_path / "guest"
    (tmp_path / "1").write_text(f"printf started > {shlex.quote(str(startup))}\n")
    step = Step("g", "j", "", f"printf ran > {shlex.quote(str(guest))}",
                jobs_flag="", hint=ResourceHint(preferred_inner_jobs=1))
    error = None
    try:
        run_dag_limited(DagConfig(steps=(step,), default_jobs_env="BASH_ENV"),
                        max_steps=1, max_cpus=1, cgroups=None, verbosity=0)
    except ValueError as exc:
        error = str(exc)
    assert error is not None and "shell startup/control variable" in error
    assert not startup.exists()
    assert not guest.exists()


def test_no_channel_configured_is_a_no_op() -> None:
    """A host that sets nothing behaves exactly as before this feature existed."""
    assert env_with_inner_jobs(_step(jobs_flag=""), "", 4) == {}


def test_no_declared_width_sets_nothing() -> None:
    assert env_with_inner_jobs(_step(jobs_flag=""), "CARGO_BUILD_JOBS", None) == {}


def test_a_step_may_opt_out_of_the_channel_specifically() -> None:
    assert env_with_inner_jobs(_step(jobs_env=""), "CARGO_BUILD_JOBS", 4) == {}


def test_step_override_beats_the_machine_default() -> None:
    assert env_with_inner_jobs(_step(jobs_env="MAKEFLAGS_J"), "CARGO_BUILD_JOBS", 2) == {
        "MAKEFLAGS_J": "2"
    }


# --- the predicate the refusal keys on -------------------------------------------------

def test_neither_channel_is_still_unresizable() -> None:
    """THE LOAD-BEARING CASE. Nothing here may make the refusal stop firing."""
    assert step_width_is_resizable(_step(jobs_flag=""), "-j", "") is False


def test_either_channel_alone_makes_it_resizable() -> None:
    assert step_width_is_resizable(_step(jobs_flag=""), "-j", "CARGO_BUILD_JOBS") is True
    assert step_width_is_resizable(_step(), "-j", "") is True


def test_whitespace_is_not_a_channel() -> None:
    """A blank-looking value must not be mistaken for a usable channel."""
    assert step_width_is_resizable(_step(jobs_flag="  "), "-j", "   ") is False


# --- resolution from the host ----------------------------------------------------------

def test_resolves_from_the_host_environment() -> None:
    assert resolve_jobs_env(env={JOBS_ENV_ENV: "CARGO_BUILD_JOBS"}) == "CARGO_BUILD_JOBS"


def test_unset_means_no_channel() -> None:
    assert resolve_jobs_env(env={}) == ""


def test_a_malformed_name_is_REFUSED_not_ignored() -> None:
    """Silently ignoring a typo would turn every self-managed step back into a hard capacity
    refusal, and the operator would debug the wrong thing."""
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        resolve_jobs_env(env={JOBS_ENV_ENV: "not a name"})


@pytest.mark.parametrize(
    "reserved",
    [
        "DAGRUN_DELEGATED_CGROUP",
        "DAGRUN_DELEGATED_UNBOXED",
        "DAGRUN_EXTRA_ARGS",
        "DAGRUN_OUTER_RUN",
        "DAGRUN_STEP",
    ],
)
def test_runner_owned_names_cannot_become_the_jobs_channel(reserved: str) -> None:
    with pytest.raises(ValueError, match="reserved by dagrun"):
        resolve_jobs_env(reserved, env={})


# --- the planner must ALLOCATE for an env-only step, not pass the raw declaration ------

def test_the_planner_allocates_width_for_an_env_only_step() -> None:
    """REGRESSION. The first version of this feature delivered the env channel at execution
    time but left `apply_plan_to_config` gating on `jobs_flag` alone, so a step resizable
    ONLY by env kept its raw `preferred_inner_jobs` instead of the planner's allocated
    width. It would have handed cargo the declared 32 while the runner believed it had
    clamped the step -- the exact "declared width that never reached the tool" defect this
    feature exists to fix, inverted.

    Five sites in estimates.py made that distinction; all now ask
    `step_width_is_resizable`, which is true for either channel.
    """
    from dagrun.estimates import apply_plan_to_config  # noqa: F401  (import must resolve)
    import inspect

    import dagrun.estimates as est

    src = inspect.getsource(est)
    # No site may gate width decisions on the flag alone any more.
    assert "effective_jobs_flag(step, cfg.default_jobs_flag).strip()" not in src, (
        "a width decision still gates on jobs_flag alone; an env-only step would be "
        "treated as unresizable and keep its raw declared width"
    )
    assert src.count("step_width_is_resizable(") >= 5

    step = _step(
        jobs_flag="",
        hint=ResourceHint(preferred_inner_jobs=4),
    )
    cfg = DagConfig(steps=(step,), default_jobs_env="CARGO_BUILD_JOBS")
    plan = build_plan(cfg, {}, planner=Planner.CPA, core_budget=1)
    assert plan.entries[0].alloc_inner_jobs == 1
    assert apply_plan_to_config(cfg, plan).steps[0].hint.preferred_inner_jobs == 1


def test_run_budget_caps_an_env_only_step() -> None:
    step = _step(jobs_flag="", hint=ResourceHint(preferred_inner_jobs=4))
    cfg = DagConfig(steps=(step,), default_jobs_env="CARGO_BUILD_JOBS")
    assert cap_config_max_cpus(cfg, 1).steps[0].hint.preferred_inner_jobs == 1


def test_programmatic_planner_and_cap_refuse_a_malformed_channel() -> None:
    malformed_step = _step(jobs_flag="-j", jobs_env="bad=name")
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        step_width_is_resizable(malformed_step, "-j", "")

    step = _step(jobs_flag="", hint=ResourceHint(preferred_inner_jobs=4))
    cfg = DagConfig(steps=(step,), default_jobs_env="bad=name")
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        step_width_is_resizable(step, cfg.default_jobs_flag, cfg.default_jobs_env)
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        cap_config_max_cpus(cfg, 1)
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        build_plan(cfg, {}, planner=Planner.CPA, core_budget=1)


# --- interchange and malformed-name refusal -------------------------------------------

def test_jobs_env_fields_round_trip() -> None:
    doc = {
        "default_jobs_env": "DEFAULT_JOBS",
        "steps": [
            {
                "group": "g",
                "job": "j",
                "cmd": "true",
                "jobs_env": "STEP_JOBS",
            }
        ],
    }
    cfg = dag_from_json(json.dumps(doc))
    assert cfg.default_jobs_env == "DEFAULT_JOBS"
    assert cfg.steps[0].jobs_env == "STEP_JOBS"
    assert json.loads(dag_to_json(cfg))["steps"][0]["jobs_env"] == "STEP_JOBS"


@pytest.mark.parametrize(
    "doc,location",
    [
        ({"default_jobs_env": "not a name", "steps": []}, "default_jobs_env"),
        (
            {
                "steps": [
                    {"group": "g", "job": "j", "cmd": "true", "jobs_env": "bad=name"}
                ]
            },
            "steps[0].jobs_env",
        ),
    ],
)
def test_malformed_document_jobs_env_is_refused(doc: object, location: str) -> None:
    with pytest.raises(DagJsonError, match=location.replace("[", r"\[").replace("]", r"\]")):
        dag_from_json(json.dumps(doc))


# --- child-observed execution ----------------------------------------------------------

class _BoxedScopeWidth(NoopCgroups):
    """A deterministic boxed boundary that reproduces the real wrapper's late export."""

    enabled = True

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        del tag, mem_max, cpu_count
        return f"export CARGO_BUILD_JOBS=8\n{cmd}"

    def cpu_stats(self, tag: str) -> dict[str, int]:
        del tag
        return {
            "usage_usec": 0,
            "user_usec": 0,
            "system_usec": 0,
            "throttled_usec": 0,
        }


class _BoxedReadonlyWidth(_BoxedScopeWidth):
    """A wrapper that makes an otherwise ordinary channel readonly before the final boundary."""

    def prepare_command(
        self,
        tag: str,
        cmd: str,
        mem_max: int | None = None,
        cpu_count: int | None = None,
    ) -> str:
        del tag, mem_max, cpu_count
        return f"readonly CARGO_BUILD_JOBS=8\n{cmd}"


def _observed_child_width(
    tmp_path: Path,
    *,
    max_cpus: int,
    default_jobs_env: str,
    cgroups: NoopCgroups | None = None,
) -> str:
    output = tmp_path / f"width-{max_cpus}-{bool(cgroups)}-{bool(default_jobs_env)}"
    step = Step(
        "g",
        "j",
        "",
        f"printf '%s' \"$CARGO_BUILD_JOBS\" > {shlex.quote(str(output))}",
        env={"CARGO_BUILD_JOBS": "99"},
        hint=ResourceHint(preferred_inner_jobs=4),
        jobs_flag="",
    )
    cfg = DagConfig(steps=(step,), default_jobs_env=default_jobs_env)
    result = run_dag_limited(
        cfg,
        max_steps=1,
        max_cpus=max_cpus,
        cgroups=cgroups,
        verbosity=0,
    )
    assert result.ok
    return output.read_text(encoding="utf-8")


def test_unboxed_child_observes_the_admitted_env_only_width(tmp_path: Path) -> None:
    assert _observed_child_width(
        tmp_path, max_cpus=1, default_jobs_env="CARGO_BUILD_JOBS"
    ) == "1"
    assert _observed_child_width(
        tmp_path, max_cpus=8, default_jobs_env="CARGO_BUILD_JOBS"
    ) == "4"


def test_boxed_child_keeps_the_per_step_width_after_the_scope_export(tmp_path: Path) -> None:
    assert _observed_child_width(
        tmp_path,
        max_cpus=1,
        default_jobs_env="CARGO_BUILD_JOBS",
        cgroups=_BoxedScopeWidth(),
    ) == "1"


def test_boxed_scope_or_operator_width_is_unchanged_without_a_jobs_env_channel(
    tmp_path: Path,
) -> None:
    assert _observed_child_width(
        tmp_path,
        max_cpus=4,
        default_jobs_env="",
        cgroups=_BoxedScopeWidth(),
    ) == "8"


@pytest.mark.parametrize(
    ("jobs_env", "cgroups"),
    [
        ("BASHOPTS", None),
        ("CARGO_BUILD_JOBS", _BoxedReadonlyWidth()),
    ],
    ids=("unboxed-bash-readonly", "boxed-wrapper-readonly"),
)
def test_unassignable_jobs_env_refuses_before_guest_command(
    tmp_path: Path,
    jobs_env: str,
    cgroups: NoopCgroups | None,
) -> None:
    marker = tmp_path / "guest-ran"
    cfg = DagConfig(
        steps=(
            Step(
                "g",
                "j",
                "",
                f"printf PASS > {shlex.quote(str(marker))}",
                hint=ResourceHint(preferred_inner_jobs=4),
                jobs_flag="",
            ),
        ),
        default_jobs_env=jobs_env,
    )
    result = run_dag_limited(
        cfg,
        max_steps=1,
        max_cpus=1,
        cgroups=cgroups,
        verbosity=0,
    )
    assert not result.ok
    assert result.outcomes[0].returncode == 125
    assert "did not retain assigned width 1" in result.outcomes[0].summary
    assert not marker.exists()


def test_programmatic_malformed_channel_refuses_before_spawn(tmp_path: Path) -> None:
    marker = tmp_path / "spawned"
    cfg = DagConfig(
        steps=(Step("g", "j", "", f"touch {shlex.quote(str(marker))}"),),
        default_jobs_env="bad=name",
    )
    with pytest.raises(ValueError, match="not a valid environment variable name"):
        Runner(cfg, max_steps=1, max_cpus=1, cgroups=NoopCgroups(), verbosity=0)
    assert not marker.exists()

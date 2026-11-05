"""`box` boxes ONE command without a DAG file.

#82 runner-box-subcommand. Boxing one ad-hoc command is this tool's primary purpose, and until
now the only way to do it was to hand-write a singleton-DAG JSON file.
"""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path

import pytest

from dagrun import cgroup as cg
from dagrun import cli
from dagrun.model import DEFAULT_STEP_TIMEOUT


def _config(argv: list[str], **kwargs: object) -> object:
    defaults: dict[str, object] = {
        "label": "probe",
        "mem_bytes": None,
        "timeout_s": 30,
        "cores": 1,
    }
    defaults.update(kwargs)
    return cli._box_step_and_config(argv, **defaults)  # type: ignore[arg-type]


def test_argv_is_shell_quoted_element_by_element_not_joined() -> None:
    """A step's cmd goes to ``bash -c``, so a joined argv makes arguments into shell SYNTAX."""
    cfg = _config(["printf", "[%s]", "a b", "it's", "semi;colon", "$(echo pwned)"])
    step = cfg.steps[0]  # type: ignore[attr-defined]
    assert step.cmd == (
        "printf '[%s]' 'a b' 'it'\"'\"'s' 'semi;colon' '$(echo pwned)'"
    )


def test_the_quoted_command_really_survives_bash(
    capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    """Quoting is only correct if bash agrees; run it and read back what the command saw."""
    code = cli.main(
        [
            "box",
            "--allow-cgroup-failure",
            "--perf-dir",
            str(tmp_path),
            "--label",
            "probe",
            "--timeout",
            "60",
            "--",
            "printf",
            "[%s]",
            "a b",
            "it's",
            "semi;colon",
            "$(echo pwned)",
        ]
    )
    out = capsys.readouterr().out
    assert code == 0, out
    assert "[a b][it's][semi;colon][$(echo pwned)]" in out, (
        "each argv element must reach the command as exactly one argument, with no shell "
        "expansion of a value that merely looks like syntax"
    )


def test_the_cpu_ceiling_is_derived_from_the_wall_ceiling_and_the_core_count() -> None:
    """Left to default, the tiny 10-second per-step CPU floor would cut an honest command short.

    That floor is a forcing function for an UNDECLARED DAG node. A user who wrote
    ``--timeout 30 --cores 4`` asked for a thirty-second wall bound, and would be astonished to
    be killed at ten CPU-seconds for a reason they never named.
    """
    cfg = _config(["true"], timeout_s=30, cores=4)
    step = cfg.steps[0]  # type: ignore[attr-defined]
    assert step.timeout == 30
    assert step.cpu_timeout == 120, "30 wall-seconds across 4 cores is 120 CPU-seconds"


def test_cores_becomes_the_cgroup_width_and_not_a_synthetic_jobs_flag() -> None:
    """``preferred_inner_jobs`` would append ``-j K`` to a command that may not accept one."""
    cfg = _config(["some-tool", "--flag"], cores=3)
    step = cfg.steps[0]  # type: ignore[attr-defined]
    assert step.hint.preferred_inner_jobs is None
    assert "-j" not in step.cmd
    assert cfg.default_step_cpu_count == 3  # type: ignore[attr-defined]


def test_mem_becomes_the_steps_own_hard_inner_cap() -> None:
    cfg = _config(["true"], mem_bytes=512 * 1024 * 1024)
    assert cfg.steps[0].hint.hard_mem_max_bytes == 512 * 1024 * 1024  # type: ignore[attr-defined]


def test_a_mem_below_the_modeled_floor_still_runs_the_command(
    capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    """512 MiB is the value the guide, the help text and #82 all show. It has to WORK.

    ``--mem`` reaches the run as ``--max-mem``, which is checked against the graph's MODELED
    worst-case footprint. That model is floored at ``mem_cap_floor_bytes`` (8 GiB by default) so
    sizing an uncharacterized graph never concludes "zero steps fit" -- and a boxed command that
    states its own hard cap is not uncharacterized. With the floor left alone, EVERY ``--mem``
    below 8 GiB exited 2 with "REFUSED — minimum runnable footprint 8589934592 bytes cannot fit
    safely within budget 536870912 bytes": the flag worked only at or above the default it exists
    to lower.
    """
    code = cli.main(
        [
            "box",
            "--allow-cgroup-failure",
            "--perf-dir",
            str(tmp_path),
            "--mem",
            "512M",
            "--timeout",
            "30",
            "--",
            "echo",
            "boxed-and-run",
        ]
    )
    captured = capsys.readouterr()
    assert "REFUSED" not in captured.err, captured.err
    assert code == 0, captured.err
    assert "boxed-and-run" in captured.out, "the command must actually have run"


def test_mem_is_also_the_ceiling_the_outer_scope_is_brought_up_with(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    """The other half of "applied TWICE": the scope's own MemoryMax, not just the step's.

    The inner cap bounds the boxed command; the outer ``MemoryMax`` bounds the whole run
    including the runner. Only the outer one is what a systemd scope is created with, so it is
    the only half a reader can check against the live unit -- and it is the half that no test
    watched: deleting the forwarding entirely left the suite green while ``--mem`` silently
    stopped reaching the scope.
    """
    seen: list[int | None] = []

    def fake_reexec(
        argv: Sequence[str],
        *,
        memory_max: int | None,
        cpu_count: int | None = None,
        naming: object = None,
        use_aggregate_slice: bool = True,
        skip_in_ci: bool = True,
        runtime_max_s: int | None = None,
    ) -> bool:
        seen.append(memory_max)
        return False  # never re-exec under test; the run then reports and exits nonzero

    monkeypatch.delenv("DAGRUN_IN_SCOPE", raising=False)
    monkeypatch.delenv(cg.DELEGATED_CGROUP_ENV, raising=False)
    monkeypatch.delenv(cg.DELEGATED_UNBOXED_ENV, raising=False)
    monkeypatch.delenv(cg.OUTER_MEMORY_MAX_ENV, raising=False)
    # A host boundary far above the request, so the requested ceiling is the one that binds.
    monkeypatch.setattr(cg, "mem_available_bytes", lambda: 64 * 1024**3)
    monkeypatch.setattr(cg, "reexec_in_scope", fake_reexec)
    monkeypatch.setattr(cg, "policy_skip_reason", lambda **_kwargs: None)

    # No --allow-cgroup-failure: this is the boxed path, which is where a scope is created.
    cli.main(
        ["box", "--perf-dir", str(tmp_path), "--mem", "512M", "--timeout", "30", "--", "true"]
    )
    assert seen == [512 * 1024 * 1024], (
        "--mem must be the MemoryMax the outer scope is created with; it never reached bring-up"
    )


def test_the_label_defaults_to_the_commands_basename_and_cannot_forge_a_group() -> None:
    """The tag is ``group.job``; a dot in the job half would read as a different group."""
    parser = cli.build_parser()
    ns = parser.parse_args(["box", "--", "/usr/bin/my.tool", "--x"])
    assert ns.label is None
    # Exercise the derivation through the command itself so the test cannot drift from it.
    cfg_holder: list[object] = []
    real = cli._run

    def capture(cfg: object, _ns: object, _c: object) -> int:
        cfg_holder.append(cfg)
        return 0

    cli._run = capture  # type: ignore[assignment]
    try:
        assert cli._cmd_box(ns, parser, cli.Palette(False)) == 0
    finally:
        cli._run = real
    assert cfg_holder[0].steps[0].tag == "box.my-tool"  # type: ignore[attr-defined]


def test_an_absent_timeout_uses_the_ordinary_step_default() -> None:
    cfg = _config(["true"], timeout_s=DEFAULT_STEP_TIMEOUT)
    assert cfg.steps[0].timeout == DEFAULT_STEP_TIMEOUT  # type: ignore[attr-defined]


def test_no_command_is_a_named_usage_error_not_a_boxed_nothing(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert cli.main(["box"]) == 2
    assert "no command given" in capsys.readouterr().err


def test_a_bad_memory_spec_is_refused_by_name(
    capsys: pytest.CaptureFixture[str], tmp_path: Path
) -> None:
    """Silently ignoring an unparseable ``--mem`` would run the command with no cap at all.

    ``--allow-cgroup-failure`` is here so that a regression FAILS rather than hanging: without
    the refusal the command would proceed to real cgroup bring-up, and a test that wedges is
    almost as unhelpful as the defect it was meant to catch.
    """
    code = cli.main(
        [
            "box",
            "--allow-cgroup-failure",
            "--perf-dir",
            str(tmp_path),
            "--mem",
            "lots",
            "--",
            "true",
        ]
    )
    err = capsys.readouterr().err
    assert code == 2, "an unparseable memory ceiling must refuse, not run uncapped"
    assert "--mem" in err and "not a positive size" in err


def test_a_nonpositive_cores_or_timeout_is_refused(
    capsys: pytest.CaptureFixture[str],
) -> None:
    with pytest.raises(SystemExit):
        cli.main(["box", "--cores", "0", "--", "true"])
    with pytest.raises(SystemExit):
        cli.main(["box", "--timeout", "0", "--", "true"])

"""CLI surface tests: argument shapes, exit codes, and the audit trail.

Exit codes are part of the contract — a caller must be able to tell "policy refused" from "the
command failed" without parsing prose — so each distinguishable outcome is asserted by code.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
from types import SimpleNamespace
from typing import cast

import pytest

from herdr_run.audit import audit_path, record
import herdr_run.cli as cli_module
import herdr_run.output as output_module
from herdr_run.cli import (
    SUBCOMMANDS,
    _default_agent,
    help_text,
    main,
    subcommand_help_text,
)
from herdr_run.config import Config
from herdr_run.evict import Survey
from herdr_run.reap import ReapDecision, ReapPlan, Verdict
from herdr_run.sweep import Occupancy
from herdr_run.errors import (
    EXIT_CONFIG,
    EXIT_REFUSED,
    ConfigError,
    HerdrRunError,
    HerdrUnavailable,
    PaneBusy,
    Refused,
    RunTimeout,
)


# --- argument shapes -------------------------------------------------------------------------------


def test_help_exits_cleanly(capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["--help"]) == 0
    captured = capsys.readouterr()
    assert captured.out.startswith("usage: herdr-run [-h] [--version]")
    assert captured.err == ""


def test_agent_is_taken_from_the_orc_environment_variable() -> None:
    assert _default_agent({"DG_AGENT_NAME": "widget-coord"}) == "widget-coord"


def test_explicit_override_beats_the_environment() -> None:
    assert (
        _default_agent({"HERDR_RUN_AGENT": "chosen", "DG_AGENT_NAME": "ambient"})
        == "chosen"
    )


def test_unknown_agent_when_nothing_is_set() -> None:
    assert _default_agent({}) == "unknown-agent"


def test_bare_invocation_prints_help_and_succeeds(
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The `make check-deps` contract: every entrypoint starts cleanly with no arguments."""

    def fail_if_config_is_loaded(*_args: object, **_kwargs: object) -> Config:
        raise AssertionError("a bare help probe must not load ambient project policy")

    monkeypatch.setattr(cli_module, "load_config", fail_if_config_is_loaded)
    assert main([]) == 0
    captured = capsys.readouterr()
    assert "usage: herdr-run" in captured.out
    assert "subcommands:" in captured.out
    assert captured.err == ""


@pytest.mark.parametrize("value", ["nan", "inf", "-1", "1e300"])
def test_invalid_cli_timeouts_are_rejected(
    value: str, capsys: pytest.CaptureFixture[str]
) -> None:
    assert main(["run", "--timeout", value, "git status"]) == 2
    assert "argument --timeout" in capsys.readouterr().err


# --- the two levels ------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "argv,fragment",
    [
        (["--cwd", "/tmp", "run", "git status"], "put it AFTER the subcommand"),
        (["--timeout", "1", "run", "git status"], "put it AFTER the subcommand"),
        (["--wait-ready", "1", "run", "git status"], "put it AFTER the subcommand"),
        (["--dry-run", "run", "git status"], "put it AFTER the subcommand"),
        (["--no-cache", "target"], "'run' or 'target'"),
        (["run", "--agent", "a", "git status"], "this is a GLOBAL option"),
        (["run", "--config", "x.yaml", "git status"], "this is a GLOBAL option"),
        (["run", "--json", "git status"], "this is a GLOBAL option"),
        (["check", "--dry-run", "git status"], "a 'run' option, not a 'check' option"),
        (["reap", "--cwd", "/tmp"], "a 'run' option, not a 'reap' option"),
        (["target", "--dry-run"], "a 'run' option, not a 'target' option"),
    ],
)
def test_an_option_at_the_wrong_level_is_refused_and_says_which_level_it_belongs_to(
    argv: list[str], fragment: str, capsys: pytest.CaptureFixture[str]
) -> None:
    """The two levels must not leak into each other, in either direction.

    This is the defect the surface exists to fix, so it is pinned as behaviour rather than left to
    the help text: an accepted `herdr-run --cwd /tmp run ...` would quietly restore the very mixing
    that made the old surface unreadable.
    """
    assert main(argv) == 2
    captured = capsys.readouterr()
    assert fragment in captured.err, captured.err
    assert captured.out == ""


@pytest.mark.parametrize(
    "argv",
    [
        ["--config", "x.yaml", "init"],
        ["--config=x.yaml", "init"],
        ["--config", "x.yaml", "init", "--force"],
        ["--config", "x.yaml", "init", "--upgrade"],
        ["--config", "x.yaml", "quickstart"],
        ["--config", "x.yaml", "userguide"],
    ],
)
def test_config_is_refused_by_the_subcommands_that_read_no_configuration(
    argv: list[str],
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: object,
) -> None:
    """A global option offered to a subcommand that cannot observe it must not be ignored.

    `--config` is not inert: it reads as an instruction about which configuration file to use, and
    `herdr-run --config P init` reads as an instruction to write `P`. It used to be accepted and
    silently disregarded, which is the same accept-anything defect the two-level surface exists to
    remove.

    Run from a scratch directory, because a regression here means `init` really runs.
    """
    monkeypatch.chdir(str(tmp_path))
    assert main(argv) == 2
    captured = capsys.readouterr()
    assert "argument --config:" in captured.err, captured.err
    assert "takes no configuration file" in captured.err, captured.err
    assert captured.out == ""


def test_a_refused_config_before_init_writes_nothing(
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: object,
) -> None:
    """The refusal is total: `init` never runs, so no configuration file appears."""
    root = str(tmp_path)
    monkeypatch.chdir(root)
    assert main(["--config", os.path.join(root, "elsewhere.yaml"), "init"]) == 2
    assert "cd DIRECTORY && herdr-run init" in capsys.readouterr().err
    assert os.listdir(root) == []


@pytest.mark.parametrize(
    "argv",
    [
        ["--agent", "a", "check", "git status"],
        ["--agent=a", "check", "git status"],
        ["--agent", "a", "init"],
        ["--agent", "a", "reap"],
        ["--agent", "a", "quickstart"],
        ["--agent", "a", "userguide"],
    ],
)
def test_agent_is_refused_by_the_subcommands_that_name_no_tab(
    argv: list[str],
    capsys: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: object,
) -> None:
    """`--agent NAME` is an instruction too: it says who this invocation speaks for.

    The one thing that does is name a tab, so a subcommand that resolves no tab has nothing to do
    with it. It used to be accepted and dropped, which reads as agreement.

    Run from a scratch directory, because a regression here means `init` really runs.
    """
    monkeypatch.chdir(str(tmp_path))
    assert main(argv) == 2
    captured = capsys.readouterr()
    assert "argument --agent:" in captured.err, captured.err
    assert "names no tab" in captured.err, captured.err
    assert captured.out == ""


@pytest.mark.parametrize(
    "argv",
    [
        ["--json", "net-doctor"],
        ["--json", "quickstart"],
        ["--json", "userguide"],
    ],
)
def test_json_is_refused_by_the_subcommands_with_no_machine_readable_output(
    argv: list[str], capsys: pytest.CaptureFixture[str]
) -> None:
    """`--json` asks for machine-readable output, and prose is not a machine-readable answer.

    A caller who is refused finds out at the flag; a caller who is humoured finds out at the parse
    of the output, where the failure looks like corrupt data instead of a bad request.
    """
    assert main(argv) == 2
    captured = capsys.readouterr()
    assert "argument --json:" in captured.err, captured.err
    assert "has no machine-readable output" in captured.err, captured.err
    assert captured.out == ""


def test_every_blind_subcommand_says_so_in_its_own_help() -> None:
    """A refusal a caller can only discover by tripping over it is a trap, and a note on a
    subcommand that accepts the option is a lie. Help and behaviour are pinned to each other.
    """
    for name, _summary in SUBCOMMANDS:
        help_ = subcommand_help_text(name)
        for table, note, label in (
            (cli_module._CONFIG_BLIND, cli_module._CONFIG_BLIND_NOTE, "--config"),
            (cli_module._AGENT_BLIND, cli_module._AGENT_BLIND_NOTE, "--agent"),
            (cli_module._JSON_BLIND, cli_module._JSON_BLIND_NOTE, "--json"),
        ):
            assert (note in help_) == (name in table), f"{name}: {label}"


def test_the_globals_the_subcommands_can_observe_are_still_accepted(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The refusals must be exactly as wide as the blindness, and no wider.

    Parsing is all that is asserted: a subcommand that got past the parse observed its globals, and
    running it for real here would need a Herdr server.
    """
    for argv in (
        ["--agent", "a", "run", "git status"],
        ["--agent", "a", "status"],
        ["--agent", "a", "config"],
        ["--agent", "a", "target"],
        ["--agent", "a", "net-doctor"],
        # Four subcommands change what they print for `--json`; three print JSON either way.
        ["--json", "run", "git status"],
        ["--json", "check", "git status"],
        ["--json", "status"],
        ["--json", "init"],
        ["--json", "config"],
        ["--json", "target"],
        ["--json", "reap"],
    ):
        cli_module._parse(argv)
        capsys.readouterr()


def test_the_subcommands_own_parse_still_wins_over_the_config_refusal(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """`--help` still helps, and a bad local option is still named as the local option it is."""
    assert main(["--config", "x.yaml", "init", "--help"]) == 0
    assert "usage: herdr-run [GLOBAL OPTIONS] init" in capsys.readouterr().out

    assert main(["--config", "x.yaml", "init", "--nonsense"]) == 2
    assert "--nonsense" in capsys.readouterr().err


def test_the_bare_command_form_is_refused_with_the_run_replacement(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The bare form is gone, and its removal names the exact replacement."""
    assert main(["git status"]) == 2
    err = capsys.readouterr().err
    assert "unknown subcommand 'git status'" in err
    assert "herdr-run run 'git status'" in err

    assert main(["release-agent", "git status"]) == 2
    err = capsys.readouterr().err
    assert "herdr-run --agent release-agent run 'git status'" in err

    assert main(["stauts"]) == 2
    err = capsys.readouterr().err
    assert "unknown subcommand 'stauts'" in err
    assert "Subcommands: " in err
    # A single word is a mistyped subcommand, not a command line; suggesting `run 'stauts'` would
    # send the reader off to debug the wrong thing.
    assert "run 'stauts'" not in err


def test_run_needs_exactly_one_quoted_command(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert main(["run"]) == 2
    assert "needs a command to run" in capsys.readouterr().err
    assert main(["run", "git", "status"]) == 2
    err = capsys.readouterr().err
    assert "ONE quoted argument" in err
    assert "herdr-run run 'git status'" in err


def test_a_double_dash_hands_the_rest_to_the_command(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.chdir(str(tmp_path))
    assert main(["run", "--dry-run", "--", "git --help"]) == 0
    assert capsys.readouterr().out == "git --help\n"
    assert main(["check", "--", "--version"]) == EXIT_REFUSED


def test_option_values_cannot_be_stolen_by_help_or_version(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert main(["--agent", "--help", "run", "x"]) == 2
    assert "argument --agent: expected one value" in capsys.readouterr().err
    assert main(["run", "--cwd", "--version", "x"]) == 2
    assert "argument --cwd: expected one value" in capsys.readouterr().err


def test_every_subcommand_is_reachable_and_uniquely_named() -> None:
    assert sorted(name for name, _summary in SUBCOMMANDS) == [
        "check",
        "config",
        "init",
        "net-doctor",
        "quickstart",
        "reap",
        "run",
        "status",
        "target",
        "userguide",
    ]
    assert all(summary for _name, summary in SUBCOMMANDS)


def _option_block(text: str, header: str) -> str:
    """Return the contiguous indented lines that follow the first line starting with `header`.

    Only the option BLOCK is inspected: an example line naming a global option is teaching where
    that option goes, which is the opposite of the mixing being guarded against.
    """
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if line.startswith(header):
            body: list[str] = []
            for candidate in lines[index + 1 :]:
                if not candidate.strip():
                    break
                body.append(candidate)
            return "\n".join(body)
    return ""


def test_each_help_level_documents_exactly_its_own_options() -> None:
    """Neither help level may document the other's options.

    The complaint that produced this surface was that every flag was global whether or not it meant
    anything to the thing being invoked, and help that lists the wrong options is how that state of
    affairs comes back without anybody deciding to bring it back.
    """
    top = help_text()
    for name, summary in SUBCOMMANDS:
        assert name in top and summary in top, name
    top_options = _option_block(top, "global options")
    for option in ("--config", "--agent", "--json", "--version", "--help"):
        assert option in top_options, option
    for local in ("--cwd", "--timeout", "--wait-ready", "--force", "--dry-run", "--no-cache"):
        assert local not in top_options, local

    run_options = _option_block(subcommand_help_text("run"), "options:")
    for local in ("--cwd", "--timeout", "--wait-ready", "--no-cache", "--dry-run"):
        assert local in run_options, local
    for global_option in ("--config", "--agent", "--json", "--version"):
        assert global_option not in run_options, global_option

    assert "--force" in _option_block(subcommand_help_text("init"), "options:")
    assert "--no-cache" in _option_block(subcommand_help_text("target"), "options:")
    for bare in ("check", "config", "reap", "net-doctor", "quickstart", "userguide", "status"):
        block = _option_block(subcommand_help_text(bare), "options:")
        assert "-h, --help" in block, bare
        for local in ("--cwd", "--timeout", "--dry-run", "--force", "--no-cache"):
            assert local not in block, f"{bare}: {local}"


def test_help_distinguishes_the_quickstart_from_the_userguide() -> None:
    """The top-level help must say what each of the two documentation commands is FOR."""
    top = help_text()
    assert "Two documentation commands" in top
    assert "one screen" in top
    assert "the complete reference" in top


def test_each_documentation_subcommand_prints_its_own_document(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert main(["quickstart"]) == 0
    quickstart = capsys.readouterr().out
    assert quickstart.startswith("# herdr-run — quickstart\n")
    assert main(["userguide"]) == 0
    guide = capsys.readouterr().out
    assert guide.startswith("# herdr-run — user guide\n")
    # Two documentation commands printing the same text would be one command with two names.
    assert quickstart != guide
    assert 2 * quickstart.count("\n") < guide.count("\n")
    assert "herdr-run userguide" in quickstart


def test_documentation_prints_even_when_the_configuration_cannot_be_parsed(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A broken config must not be able to withhold the instructions for fixing it."""
    root = str(tmp_path)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write("allow: [\n")
    monkeypatch.chdir(root)
    for subcommand in ("quickstart", "userguide"):
        assert main([subcommand]) == 0, subcommand
        assert capsys.readouterr().out.startswith("# herdr-run"), subcommand


# --- policy-only paths touch no Herdr server ---------------------------------------------------------


def test_check_allows_an_allowlisted_command(
    capsys: pytest.CaptureFixture[str],
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The built-in policy alone: a project file above the checkout must not decide the result.
    monkeypatch.chdir(str(tmp_path))
    assert main(["check", "with-proxy git ls-remote origin main"]) == 0
    out = capsys.readouterr().out
    assert "ALLOWED" in out
    assert "program=git" in out


def test_check_shows_an_invisible_or_bidirectional_character_escaped(
    capsys: pytest.CaptureFixture[str],
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The subcommand and every argument are whatever was typed. `check` used to print them raw,
    so a right-to-left override in the subcommand reordered the line on the terminal and a
    zero-width space in an argument could not be seen at all."""
    monkeypatch.chdir(str(tmp_path))
    assert main(["check", "git '\u202estatus' 'a\u200bb' caf\u00e9"]) == 0
    out = capsys.readouterr().out
    assert out == (
        'ALLOWED: program=git subcommand="\\u{202e}status"\n'
        "rendered: \"git '\\u{202e}status' 'a\\u{200b}b' 'caf\\u{e9}'\"\n"
    )
    assert main(["check", "git commit -m caf\u00e9"]) == 0
    assert capsys.readouterr().out == (
        "ALLOWED: program=git subcommand=commit\nrendered: git commit -m 'caf\u00e9'\n"
    )


def test_check_refuses_a_non_allowlisted_command(
    capsys: pytest.CaptureFixture[str],
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The built-in policy alone: an enclosing project that allows curl must not decide the result.
    monkeypatch.chdir(str(tmp_path))
    assert main(["check", "curl https://evil.example"]) == EXIT_REFUSED
    assert "REFUSED" in capsys.readouterr().err


def test_dry_run_refusal_never_reaches_a_pane(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A refused command must exit 77 without any Herdr interaction at all."""
    root = str(tmp_path)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write("spool_dir: spool\n")
    monkeypatch.chdir(root)
    pytest.importorskip("yaml")

    assert main(["--agent", "some-agent", "run", "rm -rf /"]) == EXIT_REFUSED
    assert "REFUSED" in capsys.readouterr().err


def test_dry_run_prints_the_rendered_command(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.chdir(str(tmp_path))
    assert main(["--agent", "agent", "run", "--dry-run", "git commit -m 'two words'"]) == 0
    assert "git commit -m" in capsys.readouterr().out


def test_config_subcommand_reports_the_effective_policy(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    monkeypatch.chdir(str(tmp_path))
    monkeypatch.setenv("DG_AGENT_NAME", "widget-lander")
    assert main(["config"]) == 0
    document = json.loads(capsys.readouterr().out)
    assert document["workspace"] == "agent-cmds"
    assert document["tab_label"] == "widget-lander"
    assert document["allow"] == ["git", "gh"]
    assert document["max_panes"] == 32


def test_reap_subcommand_reports_both_sides_and_closes_nothing(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The production caller for the reaping policy. Report-only, and counted on both sides."""
    root = str(tmp_path)
    monkeypatch.chdir(root)
    decisions = (
        ReapDecision("w1:p1", "w1:t1", "kvm", Verdict.STALE, "shell is gone"),
        ReapDecision("w1:p2", "w1:t2", "amd", Verdict.SHELL_ALIVE, "still the original process"),
    )
    monkeypatch.setattr(cli_module, "_client", lambda *_args: object())
    monkeypatch.setattr(cli_module, "sweep", lambda *_args, **_kwargs: ReapPlan(decisions))
    monkeypatch.setattr(
        cli_module, "measure_occupancy", lambda *_args: Occupancy(live_panes=5, with_record=2, listing_error=None)
    )
    monkeypatch.setattr(
        cli_module, "survey_workspace", lambda *_args: Survey(replaceable=["w1:p1"], kept=[("w1:p2", "running: git")])
    )

    assert main(["reap"]) == 0
    document = json.loads(capsys.readouterr().out)
    assert document["counts"]["STALE"] == 1
    assert document["counts"]["SHELL_ALIVE"] == 1
    # Both halves are printed: a report listing only what it would close cannot be audited for
    # the refusals, which are the expensive direction to get wrong.
    assert [entry["pane_id"] for entry in document["reapable"]] == ["w1:p1"]
    assert [entry["pane_id"] for entry in document["declined"]] == ["w1:p2"]
    assert all(entry["reason"] for entry in document["declined"])
    # The report also has to state the bound on what it could POSSIBLY have considered. Candidates
    # are the panes named by surviving run records, and herdr-run's own retention deletes those
    # records -- so the oldest leaked tabs, the ones the pane cap exists to bound, are exactly the
    # ones missing from this count. Printing the window keeps "considered: 2" from implying more.
    assert document["candidate_source"]["retention_days"] == 4
    assert "retention_days" in document["candidate_source"]["note"]
    # And what holds max_panes: the cap counts every pane herdr lists, the verdicts above cover only
    # the panes a record names, so the panes nobody can judge are counted rather than left implicit.
    occupancy = document["occupancy"]
    assert (occupancy["live_panes"], occupancy["with_record"], occupancy["without_record"]) == (5, 2, 3)
    assert occupancy["max_panes"] == Config().max_panes
    assert occupancy["listing_error"] is None
    assert "max_panes" in occupancy["note"]
    # Whether the cap would still admit the next agent: one tab is idle and replaceable.
    assert (occupancy["replaceable"], occupancy["cap_refusal"], occupancy["survey_error"]) == (1, None, None)


def _reap_occupancy(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], occupancy: Occupancy, survey: object
) -> dict[str, object]:
    def fake_survey(*_args: object) -> Survey:
        if isinstance(survey, Exception):
            raise survey
        assert isinstance(survey, Survey), "the survey must not run after a failed listing"
        return survey

    monkeypatch.setattr(cli_module, "_client", lambda *_args: object())
    monkeypatch.setattr(cli_module, "sweep", lambda *_args, **_kwargs: ReapPlan(()))
    monkeypatch.setattr(cli_module, "measure_occupancy", lambda *_args: occupancy)
    monkeypatch.setattr(cli_module, "survey_workspace", fake_survey)
    assert main(["reap"]) == 0
    return cast(dict[str, object], json.loads(capsys.readouterr().out)["occupancy"])


def test_reap_names_the_cap_refusal_when_no_tab_is_replaceable(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.chdir(str(tmp_path))
    kept = [("w1:p1", "running: git"), ("w1:p2", "tab w1:t2 holds 2 panes; only an unsplit tab is replaced")]
    occupancy = _reap_occupancy(
        monkeypatch, capsys, Occupancy(live_panes=2, with_record=0, listing_error=None), Survey([], kept)
    )
    assert occupancy["replaceable"] == 0
    assert occupancy["cap_refusal"] == (
        "None of its 2 tab(s) could be replaced: pane w1:p1: running: git; "
        "pane w1:p2: tab w1:t2 holds 2 panes; only an unsplit tab is replaced."
    )
    assert occupancy["survey_error"] is None


def test_reap_reports_a_failed_survey_as_unknown_not_zero(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.chdir(str(tmp_path))
    occupancy = _reap_occupancy(
        monkeypatch,
        capsys,
        Occupancy(live_panes=2, with_record=0, listing_error=None),
        HerdrUnavailable("pane list: herdr is not answering"),
    )
    assert (occupancy["replaceable"], occupancy["cap_refusal"]) == (None, None)
    assert occupancy["survey_error"] == "pane list: herdr is not answering"


def test_reap_does_not_survey_after_a_failed_listing(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.chdir(str(tmp_path))
    occupancy = _reap_occupancy(
        monkeypatch, capsys, Occupancy(live_panes=None, with_record=None, listing_error="no herdr"), None
    )
    assert (occupancy["replaceable"], occupancy["cap_refusal"], occupancy["survey_error"]) == (None, None, None)
    assert occupancy["listing_error"] == "no herdr"


class _CapturedText(io.StringIO):
    """Text stream exposing the binary buffer used by a normal terminal stream."""

    def __init__(self) -> None:
        super().__init__()
        self.raw = io.BytesIO()

    @property
    def buffer(self) -> io.BytesIO:
        return self.raw


def test_raw_result_streams_are_emitted_byte_exactly_once(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    stdout = _CapturedText()
    stderr = _CapturedText()
    monkeypatch.setattr(sys, "stdout", stdout)
    monkeypatch.setattr(sys, "stderr", stderr)

    output_module.stdout_bytes(b"A\x00\xff")
    output_module.stderr_bytes(b"B\r\n")

    assert stdout.raw.getvalue() == b"A\x00\xff"
    assert stderr.raw.getvalue() == b"B\r\n"


@pytest.mark.parametrize("stream_name", ["stdout", "stderr"])
def test_raw_output_write_failure_is_a_typed_wrapper_error(
    stream_name: str,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class _BrokenBuffer:
        def write(self, data: bytes) -> int:
            del data
            raise OSError("output pipe closed")

    stream = _CapturedText()
    stream.raw = cast(io.BytesIO, _BrokenBuffer())
    monkeypatch.setattr(sys, stream_name, stream)

    with pytest.raises(HerdrRunError, match=f"cannot write {stream_name}: output pipe closed"):
        if stream_name == "stdout":
            output_module.stdout_bytes(b"output")
        else:
            output_module.stderr_bytes(b"error")


def test_timeout_partial_output_write_failure_has_no_traceback(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = str(tmp_path)
    monkeypatch.chdir(root)
    target = SimpleNamespace(pane_id="p1", tab_label="agent")
    timed_out = RunTimeout("still running")
    timed_out.partial_stdout = "partial\n"
    timed_out.partial_stderr = ""
    monkeypatch.setattr(cli_module, "_client", lambda *_args: object())
    monkeypatch.setattr(cli_module, "resolve_target", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(cli_module, "execute", lambda *_args, **_kwargs: _raise(timed_out))
    monkeypatch.setattr(
        output_module,
        "write_labelled",
        lambda _descriptor, _payload, label: _raise(
            output_module.OutputError(f"cannot write {label}: closed")
        ),
    )

    assert main(["--agent", "agent", "run", "git status"]) == 1
    captured = capsys.readouterr()
    assert "herdr-run: cannot write partial stdout: closed" in captured.err
    assert "Traceback" not in captured.err


def test_metadata_failure_does_not_mask_completed_command(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = str(tmp_path)
    monkeypatch.chdir(root)
    target = SimpleNamespace(
        pane_id="p1", tab_label="agent", created=(), tab_id="t1", workspace_id="w1"
    )
    result = SimpleNamespace(
        exit_code=23,
        stdout="output",
        stderr="",
        run_id="run-1",
        spool=SimpleNamespace(
            directory=os.path.join(root, ".herdr-run", "runs", "run-1")
        ),
        target=target,
        duration_seconds=0.25,
    )
    monkeypatch.setattr(cli_module, "_client", lambda *_args: object())
    monkeypatch.setattr(cli_module, "resolve_target", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(cli_module, "execute", lambda *_args, **_kwargs: result)
    monkeypatch.setattr(
        cli_module,
        "write_meta",
        lambda *_args: (_ for _ in ()).throw(
            HerdrUnavailable("cannot write run metadata /x: Read-only file system (os error 30)")
        ),
    )
    monkeypatch.setattr(
        cli_module, "read_output_bytes", lambda _result: (b"output", b"")
    )

    assert main(["--json", "--agent", "agent", "run", "git status"]) == 23
    captured = capsys.readouterr()
    assert json.loads(captured.out)["meta"] is None
    assert "WARNING: cannot write run metadata" in captured.err
    with open(audit_path(root, ".herdr-run"), encoding="utf-8") as handle:
        entries = [json.loads(line) for line in handle]
    assert [entry["verdict"] for entry in entries] == ["ADMITTED", "RAN"]
    assert entries[-1]["meta"] is None
    assert "meta_error" in entries[-1]


def _raise(error: Exception) -> object:
    raise error


@pytest.mark.parametrize(
    ("failure_site", "error", "verdict"),
    [
        ("client", HerdrUnavailable("client unavailable"), "HERDRUNAVAILABLE"),
        ("target", HerdrUnavailable("target unavailable"), "HERDRUNAVAILABLE"),
        ("cwd", ConfigError("caller cwd vanished"), "CONFIGERROR"),
        ("execute", PaneBusy("pane stayed busy"), "PANEBUSY"),
    ],
)
def test_net_doctor_pane_failures_keep_its_output_exit_and_audit_shape(
    failure_site: str,
    error: Exception,
    verdict: str,
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Every failure in the pane half is a failed diagnosis, not a typed early exit."""
    root = str(tmp_path)
    monkeypatch.chdir(root)

    def direct_probe(
        command: object, *, timeout: float
    ) -> SimpleNamespace:
        assert command == [
            "/bin/bash",
            "-lc",
            "with-proxy git ls-remote https://github.com/git/git HEAD",
        ]
        assert timeout == 120
        return SimpleNamespace(returncode=1, stdout="", stderr="blocked in jail\n")

    monkeypatch.setattr(
        cli_module,
        "bounded_control_command",
        direct_probe,
    )
    target = SimpleNamespace(pane_id="p1", tab_label="agent")
    monkeypatch.setattr(
        cli_module,
        "_client",
        lambda *_args: _raise(error) if failure_site == "client" else object(),
    )
    monkeypatch.setattr(
        cli_module,
        "resolve_target",
        lambda *_args, **_kwargs: (
            _raise(error) if failure_site == "target" else target
        ),
    )
    monkeypatch.setattr(
        cli_module,
        "_resolved_cwd",
        lambda *_args: _raise(error) if failure_site == "cwd" else root,
    )
    monkeypatch.setattr(
        cli_module,
        "execute",
        lambda *_args, **_kwargs: _raise(error),
    )

    assert main(["net-doctor"]) == 1
    captured = capsys.readouterr()
    assert f"[via pane] FAILED: {error}" in captured.out
    assert "VERDICT: the pane path is NOT working." in captured.out
    assert "Traceback" not in captured.err
    # The scope disclaimer leads, so a reader whose interest is something else can stop at line
    # one instead of reading a verdict about a scenario they are not in.
    assert captured.out.startswith("herdr-run net-doctor  (agent=")
    assert "This is a smoke test for ONE scenario" in captured.out
    assert captured.out.index("ONE scenario") < captured.out.index("[direct  ]")
    assert "[in-jail ]" not in captured.out

    with open(audit_path(root, ".herdr-run"), encoding="utf-8") as handle:
        entries = [json.loads(line) for line in handle]
    assert [entry["verdict"] for entry in entries] == ["ADMITTED", verdict]
    assert entries[-1]["net_doctor"] is True
    assert entries[-1]["direct_exit_code"] == 1


def test_net_doctor_pre_admission_refusal_is_marked_as_net_doctor(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = str(tmp_path)
    monkeypatch.chdir(root)
    monkeypatch.setattr(
        cli_module, "admit", lambda *_args: _raise(Refused("probe refused"))
    )

    assert main(["net-doctor"]) == EXIT_REFUSED
    assert "herdr-run: probe refused" in capsys.readouterr().err
    with open(audit_path(root, ".herdr-run"), encoding="utf-8") as handle:
        entry = json.loads(handle.read())
    assert entry["verdict"] == "REFUSED"
    assert entry["net_doctor"] is True


def test_vanished_caller_cwd_is_a_typed_config_error(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Model getcwd(3)'s ENOENT after another process removes the caller's directory."""
    monkeypatch.setattr(
        os,
        "getcwd",
        lambda: _raise(FileNotFoundError("caller directory was removed")),
    )

    assert main(["--agent", "agent", "run", "git status"]) == EXIT_CONFIG
    captured = capsys.readouterr()
    assert "cannot determine current directory" in captured.err
    assert "Traceback" not in captured.err


def test_a_configuration_failure_after_admission_is_audited(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A run that dies AFTER the policy said yes must still leave both halves in the log.

    An audit that recorded only the admission would say a command was authorised and never say
    what became of it, which is the one thing the log exists to answer.
    """
    root = str(tmp_path)
    config = Config(project_root=root)
    monkeypatch.setattr(cli_module, "load_config", lambda **_kwargs: config)
    monkeypatch.setattr(cli_module, "_client", lambda *_args: object())
    monkeypatch.setattr(
        cli_module,
        "_resolved_cwd",
        lambda *_args: _raise(ConfigError("caller directory was removed")),
    )

    assert main(["--agent", "agent", "run", "git status"]) == EXIT_CONFIG
    captured = capsys.readouterr()
    assert "caller directory was removed" in captured.err
    assert "Traceback" not in captured.err
    with open(audit_path(root, ".herdr-run"), encoding="utf-8") as handle:
        entries = [json.loads(line) for line in handle]
    assert [entry["verdict"] for entry in entries] == ["ADMITTED", "CONFIGERROR"]


# --- audit trail ---------------------------------------------------------------------------------------


def test_refusals_are_audited(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A log that recorded only successes would make the allowlist unobservable after the fact."""
    root = str(tmp_path)
    monkeypatch.chdir(root)
    main(["--agent", "agent", "run", "curl https://evil.example"])

    path = audit_path(root, ".herdr-run")
    with open(path, encoding="utf-8") as handle:
        entries = [json.loads(line) for line in handle if line.strip()]
    assert len(entries) == 1
    assert entries[0]["verdict"] == "REFUSED"
    assert entries[0]["command"] == "curl https://evil.example"
    assert entries[0]["agent"] == "agent"


def test_audit_appends_rather_than_truncating(tmp_path: object) -> None:
    path = os.path.join(str(tmp_path), "audit.jsonl")
    os.chmod(str(tmp_path), 0o755)
    assert record(path, agent="a", command="git status", verdict="RAN", detail="exit 0")
    assert record(path, agent="b", command="curl x", verdict="REFUSED", detail="nope")
    with open(path, encoding="utf-8") as handle:
        entries = [json.loads(line) for line in handle if line.strip()]
    assert [entry["verdict"] for entry in entries] == ["RAN", "REFUSED"]
    assert os.stat(path).st_mode & 0o777 == 0o600
    assert os.stat(str(tmp_path)).st_mode & 0o777 == 0o755


def test_unwritable_audit_log_does_not_raise() -> None:
    """The audit log must never be the reason a run fails."""
    assert not record(
        "/proc/definitely/not/writable/audit.jsonl",
        agent="a",
        command="c",
        verdict="RAN",
        detail="d",
    )


# --- spool hygiene: command output must not land where `git add` can pick it up ---------------------


def test_spool_ignore_probe_uses_bounded_group_runner(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import herdr_run.audit as audit_module

    def bounded(command: list[str], *, timeout: float) -> SimpleNamespace:
        assert command == [
            "git",
            "-C",
            "/repo",
            "check-ignore",
            "-q",
            "--",
            "/repo/.herdr-run/probe",
        ]
        assert timeout == 10
        return SimpleNamespace(returncode=1, stdout="", stderr="")

    monkeypatch.setattr(audit_module, "bounded_control_command", bounded)
    assert audit_module.spool_is_ignored("/repo", ".herdr-run") is False


def test_warns_when_the_spool_dir_is_not_gitignored(
    tmp_path: object, capsys: pytest.CaptureFixture[str]
) -> None:
    """Negative bracket: an un-ignored spool inside a real repo must produce a warning."""
    import subprocess

    from herdr_run.audit import spool_is_ignored, warn_if_spool_is_tracked

    root = str(tmp_path)
    subprocess.run(["git", "init", "-q", root], check=True, capture_output=True)

    assert spool_is_ignored(root, ".herdr-run") is False
    assert warn_if_spool_is_tracked(root, ".herdr-run") is True
    assert "NOT git-ignored" in capsys.readouterr().err


def test_stays_quiet_when_the_spool_dir_is_gitignored(
    tmp_path: object, capsys: pytest.CaptureFixture[str]
) -> None:
    """Positive bracket: with the ignore rule present the warning must NOT fire (not merely inert)."""
    import os
    import subprocess

    from herdr_run.audit import spool_is_ignored, warn_if_spool_is_tracked

    root = str(tmp_path)
    subprocess.run(["git", "init", "-q", root], check=True, capture_output=True)
    with open(os.path.join(root, ".gitignore"), "w", encoding="utf-8") as handle:
        handle.write(".herdr-run/\n")

    assert spool_is_ignored(root, ".herdr-run") is True
    assert warn_if_spool_is_tracked(root, ".herdr-run") is False
    assert capsys.readouterr().err == ""


def test_no_claim_outside_a_git_work_tree(tmp_path: object) -> None:
    """Outside a repo the question does not apply, so report None rather than inventing a finding."""
    from herdr_run.audit import spool_is_ignored

    assert spool_is_ignored(str(tmp_path), ".herdr-run") is None


def test_run_options_are_accepted_after_the_subcommand(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Every `run` option belongs after `run`, in any order, including attached-value forms."""
    monkeypatch.chdir(str(tmp_path))
    assert (
        main(
            [
                "--agent",
                "agent",
                "run",
                "--cwd",
                "/tmp",
                "--timeout=12.5",
                "--wait-ready",
                "3",
                "--no-cache",
                "--dry-run",
                "git status",
            ]
        )
        == 0
    )
    assert capsys.readouterr().out == "git status\n"


def test_loose_argv_words_are_refused_not_silently_rejoined(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Re-joining loose words would silently destroy quoting; refuse instead.

    `herdr-run run git commit -m "two words"` would otherwise arrive at git as `-m two words`,
    and nothing in the output would reveal that the caller's quoting had been discarded.
    """
    monkeypatch.chdir(str(tmp_path))
    assert main(["run", "git", "status", "--", "path with spaces"]) == 2
    err = capsys.readouterr().err
    assert "ONE quoted argument" in err
    assert "silently change the quoting" in err


def test_loose_argv_containing_an_unknown_flag_is_also_refused(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The other loose shape: the flag is refused before the command is ever assembled.

    Recorded because the two shapes fail through DIFFERENT paths and both must be non-silent:
    `-m` is not a `run` option, so parsing stops there.
    """
    monkeypatch.chdir(str(tmp_path))
    assert main(["run", "git", "commit", "-m", "two words"]) == 2
    assert "run: unrecognized arguments: -m" in capsys.readouterr().err


def test_the_documented_form_still_works(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Positive control: a global agent plus one quoted command is the shape, and must not regress."""
    monkeypatch.chdir(str(tmp_path))
    assert (
        main(
            [
                "--agent",
                "someagent",
                "run",
                "--dry-run",
                "with-proxy git ls-remote origin main",
            ]
        )
        == 0
    )
    assert "with-proxy git ls-remote origin main" in capsys.readouterr().out


def test_a_leading_wrapper_is_never_swallowed_as_an_agent_name(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """`run with-proxy '<cmd>'` must NOT drop `with-proxy`.

    The old surface did: with two positionals the leading one was discarded from the command even
    when --agent had already supplied the name. The command still ran, minus its proxy wrapper, so
    `gh` dialled GitHub directly and reported "network is unreachable" -- indistinguishable from a
    real egress outage, and the reason a whole fleet concluded `gh` was unusable through the pane.
    Now nothing before the command can be read as an agent name at all, and two positionals are
    refused with the wrapper put back INSIDE the quotes.
    """
    monkeypatch.chdir(str(tmp_path))
    assert (
        main(
            [
                "--agent",
                "someagent",
                "run",
                "--dry-run",
                "with-proxy",
                "git ls-remote origin",
            ]
        )
        == 2
    )
    err = capsys.readouterr().err
    assert "ONE quoted argument" in err
    assert "'with-proxy git ls-remote origin'" in err


def test_command_runs_in_the_callers_cwd_not_the_config_directory(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Regression: a config file ABOVE the caller must not relocate where the command runs.

    Config discovery walks up to the nearest .herdr-run.yaml, so a file at the top of a multi-repo
    tree used to make every nested worktree run its command in that top directory instead of the
    one the caller was standing in -- silent mistargeting that reads as the wrong repo answering.
    """
    import os

    from herdr_run.cli import _resolved_cwd
    from herdr_run.config import Config

    root = str(tmp_path)
    nested = os.path.join(root, "worktrees", "slot", "repo")
    os.makedirs(nested)
    config = Config(project_root=root)  # config lives at the top...
    monkeypatch.chdir(nested)  # ...caller stands in the nested worktree

    assert _resolved_cwd(config, None) == os.path.realpath(nested) or (
        _resolved_cwd(config, None) == nested
    )


def test_explicit_cwd_still_wins(tmp_path: object) -> None:
    """Positive control: --cwd is still honoured over both the config and the caller's cwd."""
    from herdr_run.cli import _resolved_cwd
    from herdr_run.config import Config

    target = str(tmp_path)
    assert _resolved_cwd(Config(project_root="/elsewhere"), target) == target


def test_explicit_empty_cwd_means_the_project_root() -> None:
    """An explicitly supplied empty value is not the same as omitting --cwd."""
    from herdr_run.cli import _resolved_cwd
    from herdr_run.config import Config

    assert _resolved_cwd(Config(project_root="/project", cwd="configured"), "") == "/project"


# --- the process the interpreter hands over ---------------------------------------------------


def test_the_utf8_restart_takes_the_command_line_and_environment_from_proc(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The restart passes the original bytes on; nothing the locale decoded is encoded back.

    Under a multibyte locale such as ja_JP.eucJP, encoding ``sys.argv`` back raised
    UnicodeEncodeError for an argument holding UTF-8 text, before any output.
    """
    command = [b"/usr/bin/python3", b"-m", b"herdr_run", b"check", "git 日本".encode(), b"\xff", b""]
    environment = [
        b"LC_ALL=ja_JP.eucJP",
        b"=A=1",
        b"HOME=/h",
        b"NOEQUALS",
        b"=empty-name",
        b"VALUE=a=b",
        b"HOME=/second",
    ]
    captured: dict[str, object] = {}

    def strings(name: str) -> list[bytes]:
        return command if name == "cmdline" else environment

    def execve(path: str, argv: list[object], env: dict[bytes, bytes]) -> None:
        captured.update(path=path, argv=argv, env=env)

    monkeypatch.setattr(cli_module, "_proc_self_strings", strings)
    monkeypatch.setattr(os, "execve", execve)
    # What the locale made of the arguments must not matter: these cannot be encoded back at all.
    monkeypatch.setattr(sys, "argv", ["herdr-run", "check", "git \udc80\ud800"])
    cli_module._restart_in_utf8_mode(["-m", "herdr_run"])
    assert captured["argv"] == [command[0], b"-X", b"utf8", *command[1:]]
    # The shared rule: the first of two entries with one name is kept where it stood, and an
    # entry without '=' or with an empty name (``=A=1`` included) is dropped. Nothing is added, so
    # PEP 538's LC_CTYPE cannot reach the restarted process.
    env = captured["env"]
    assert isinstance(env, dict)
    assert list(env.items()) == [
        (b"LC_ALL", b"ja_JP.eucJP"),
        (b"HOME", b"/h"),
        (b"VALUE", b"a=b"),
    ]


def test_the_utf8_restart_without_proc_passes_this_environment_under_the_rule(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Without ``/proc`` the environment is still passed explicitly, never inherited as it is."""
    from herdr_run.client import child_environment, environment_entries

    captured: dict[str, object] = {}
    monkeypatch.setattr(sys, "_xoptions", {})
    monkeypatch.setattr(cli_module, "_proc_self_strings", lambda name: None)
    monkeypatch.setattr(os, "execv", lambda *args: pytest.fail("the environment was inherited"))
    monkeypatch.setattr(os, "execve", lambda path, argv, env: captured.update(env=env))
    monkeypatch.setenv("HERDR_RUN_TEST_RESTART", "kept=as=is")
    cli_module._restart_in_utf8_mode(["-m", "herdr_run"])
    assert captured["env"] == child_environment(environment_entries())
    env = captured["env"]
    assert isinstance(env, dict) and env[b"HERDR_RUN_TEST_RESTART"] == b"kept=as=is"


def test_the_utf8_restart_is_skipped_when_the_interpreter_turned_utf8_mode_off(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(sys, "_xoptions", {"utf8": "0"})
    monkeypatch.setattr(
        cli_module, "_proc_self_strings", lambda name: [b"python3", b"-X", b"utf8=0", b"-m"]
    )
    monkeypatch.setattr(os, "execve", lambda *args: pytest.fail("restarted into a loop"))
    cli_module._restart_in_utf8_mode(["-m", "herdr_run"])


def test_an_argument_that_reads_like_the_utf8_option_does_not_stop_the_restart(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Only the interpreter's own ``-X`` options count; herdr-run's arguments are not options."""
    command = [b"python3", b"-m", b"herdr_run", b"check", b"--", b"-Xutf8=0", b"-X", b"utf8=0"]
    captured: dict[str, object] = {}
    monkeypatch.setattr(sys, "_xoptions", {})
    monkeypatch.setattr(
        cli_module, "_proc_self_strings", lambda name: command if name == "cmdline" else []
    )
    monkeypatch.setattr(os, "execve", lambda path, argv, env: captured.update(argv=argv))
    cli_module._restart_in_utf8_mode(["-m", "herdr_run"])
    assert captured["argv"] == [command[0], b"-X", b"utf8", *command[1:]]


def test_the_environment_is_put_back_to_the_one_executed_with(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("LC_CTYPE", "C.UTF-8")
    monkeypatch.setenv("HERDR_RUN_TEST_KEPT", "changed")
    monkeypatch.delenv("HERDR_RUN_TEST_GONE", raising=False)
    original = dict(os.environb)
    del original[b"LC_CTYPE"]
    original[b"HERDR_RUN_TEST_KEPT"] = b"original"
    original[b"HERDR_RUN_TEST_GONE"] = b"\xffback"
    cli_module._restore_environment(original)
    assert "LC_CTYPE" not in os.environ
    assert os.environ["HERDR_RUN_TEST_KEPT"] == "original"
    assert os.environb[b"HERDR_RUN_TEST_GONE"] == b"\xffback"
    assert dict(os.environb) == original


def test_proc_strings_keep_empty_arguments_and_an_empty_environment(
    monkeypatch: pytest.MonkeyPatch, tmp_path: object
) -> None:
    payloads = {"cmdline": b"python3\0-m\0herdr_run\0check\0\0", "environ": b""}

    class Handle(io.BytesIO):
        def __enter__(self) -> "Handle":
            return self

        def __exit__(self, *exc: object) -> None:
            self.close()

    def fake_open(path: str, mode: str = "r", *args: object, **kwargs: object) -> Handle:
        assert mode == "rb"
        return Handle(payloads[path.rsplit("/", 1)[1]])

    monkeypatch.setattr("builtins.open", fake_open)
    assert cli_module._proc_self_strings("cmdline") == [b"python3", b"-m", b"herdr_run", b"check", b""]
    assert cli_module._proc_self_strings("environ") == []


def test_a_short_buffered_write_is_retried_until_it_fails_or_completes() -> None:
    """A buffer may report fewer bytes than it was given without raising; the rest is retried."""

    class ShortBuffer:
        def __init__(self, failure: OSError | None) -> None:
            self.written = b""
            self.failure = failure

        def write(self, data: memoryview) -> int:
            if self.written and self.failure is not None:
                raise self.failure
            chunk = bytes(data[:3])
            self.written += chunk
            return len(chunk)

        def flush(self) -> None:
            pass

    class Stream:
        def __init__(self, buffer: ShortBuffer) -> None:
            self.buffer = buffer

        def flush(self) -> None:
            pass

    complete = ShortBuffer(None)
    output_module._write(cast("io.TextIOWrapper", Stream(complete)), 1, b"0123456789")
    assert complete.written == b"0123456789"

    full = ShortBuffer(OSError(27, "File too large"))
    with pytest.raises(OSError, match="File too large"):
        output_module._write(cast("io.TextIOWrapper", Stream(full)), 1, b"0123456789")


# --- the process as a caller starts it ---------------------------------------------------------

_PACKAGE_PARENT = os.path.dirname(os.path.dirname(os.path.abspath(cli_module.__file__)))


def _clean_environment() -> dict[str, str]:
    environment = {key: value for key, value in os.environ.items() if not key.startswith("PYTHON")}
    environment["PYTHONPATH"] = _PACKAGE_PARENT
    return environment


@pytest.mark.parametrize("unbuffered", [None, "1"])
def test_a_failed_stdout_write_leaves_nothing_for_shutdown_to_retry(unbuffered: str | None) -> None:
    """One diagnostic and exit 1, whatever PYTHONUNBUFFERED says.

    A buffered write that failed kept its bytes; interpreter shutdown flushed them again, printed
    ``Exception ignored`` and exited 120, unless PYTHONUNBUFFERED was set.
    """
    import subprocess

    environment = _clean_environment()
    if unbuffered is not None:
        environment["PYTHONUNBUFFERED"] = unbuffered
    with open("/dev/full", "wb") as full:
        completed = subprocess.run(
            [sys.executable, "-m", "herdr_run", "--version"],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=full,
            stderr=subprocess.PIPE,
            check=False,
            timeout=60,
        )
    assert completed.returncode == 1
    assert (
        completed.stderr
        == b"herdr-run: cannot write stdout: No space left on device (os error 28)\n"
    )


def test_a_stderr_nobody_reads_does_not_change_the_exit_status() -> None:
    import subprocess

    reader, writer = os.pipe()
    os.close(reader)
    try:
        completed = subprocess.run(
            [sys.executable, "-m", "herdr_run", "check", "curl x"],
            env=_clean_environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=writer,
            check=False,
            timeout=60,
        )
    finally:
        os.close(writer)
    assert completed.returncode == EXIT_REFUSED
    assert completed.stdout == b""


def test_an_inherited_ignored_sigchld_is_reset_so_child_statuses_are_real(tmp_path: object) -> None:
    """With SIGCHLD ignored, ``subprocess`` reads every child's status as 0.

    ``git check-ignore`` then said "ignored" for a spool that is not, and the warning was lost.
    """
    import signal
    import subprocess

    root = str(tmp_path)
    subprocess.run(["git", "init", "-q", root], check=True, capture_output=True)
    environment = _clean_environment()
    environment["GIT_CONFIG_NOSYSTEM"] = "1"
    completed = subprocess.run(
        [sys.executable, "-m", "herdr_run", "--agent", "a", "run", "--dry-run", "git status"],
        cwd=root,
        env=environment,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        check=False,
        timeout=60,
        preexec_fn=lambda: signal.signal(signal.SIGCHLD, signal.SIG_IGN),
    )
    assert completed.returncode == 0, completed.stderr
    assert b"is NOT git-ignored" in completed.stderr


#: What ``pip`` writes for the ``herdr-run = herdr_run.cli:main`` console script.
_CONSOLE_SCRIPT = """\
# -*- coding: utf-8 -*-
import re
import sys
from herdr_run.cli import main
if __name__ == '__main__':
    sys.argv[0] = re.sub(r'(-script\\.pyw|\\.exe)?$', '', sys.argv[0])
    sys.exit(main())
"""

#: Interrupts the process as the first package module after ``herdr_run`` itself is looked for.
_INTERRUPT_AFTER_THE_RESET = """\
import os, signal, sys

class _Interrupt:
    @staticmethod
    def find_spec(name, path=None, target=None):
        if name == "herdr_run.cli":
            os.kill(os.getpid(), signal.SIGINT)
        return None

sys.meta_path.insert(0, _Interrupt)
"""

#: Stands in for an interrupt that arrives inside the reset itself: the first ``getsignal`` the
#: entry point makes raises ``KeyboardInterrupt``, as the interpreter would at that instruction.
_INTERRUPT_INSIDE_THE_RESET = """\
import os, sys, types

_real = sys.modules["_signal"]

class _Signal(types.ModuleType):
    raised = False

    def __getattr__(self, name):
        return getattr(_real, name)

    def getsignal(self, number):
        if not _Signal.raised:
            _Signal.raised = True
            open(os.environ["HERDR_RUN_TEST_MARKER"], "w").close()
            raise KeyboardInterrupt
        return _real.getsignal(number)

sys.modules["_signal"] = _Signal("_signal")
"""


def _entry_points(directory: str) -> dict[str, list[str]]:
    """The three ways herdr-run is started, each running ``--version``."""
    console = os.path.join(directory, "herdr-run")
    with open(console, "w", encoding="utf-8") as handle:
        handle.write(_CONSOLE_SCRIPT)
    script = os.path.join(_PACKAGE_PARENT, "bin", "herdr-run")
    return {
        "module": [sys.executable, "-m", "herdr_run", "--version"],
        "console-script": [sys.executable, console, "--version"],
        "bin-script": [sys.executable, script, "--version"],
    }


def _ignore_sigint() -> None:
    import signal

    signal.signal(signal.SIGINT, signal.SIG_IGN)


def _start(
    argv: list[str], customize: str, directory: str, *, ignore_sigint: bool = False
) -> "subprocess.CompletedProcess[bytes]":
    site = os.path.join(directory, "site")
    os.makedirs(site, exist_ok=True)
    with open(os.path.join(site, "sitecustomize.py"), "w", encoding="utf-8") as handle:
        handle.write(customize)
    environment = _clean_environment()
    environment["PYTHONPATH"] = os.pathsep.join((site, _PACKAGE_PARENT))
    environment["HERDR_RUN_TEST_MARKER"] = os.path.join(directory, "raised")
    return subprocess.run(
        argv,
        env=environment,
        capture_output=True,
        check=False,
        timeout=60,
        preexec_fn=_ignore_sigint if ignore_sigint else None,
    )


@pytest.mark.parametrize("entry_point", ["module", "console-script", "bin-script"])
def test_every_entry_point_resets_sigint_before_any_other_package_code(
    entry_point: str, tmp_path: object
) -> None:
    """An interrupt once the package is importing ends the process by SIGINT, with no traceback.

    The guard in ``__main__`` alone did not cover the console script, which never imports
    ``__main__``, nor its own script form, which imported ``signal`` and ``typing`` first.
    """
    import signal

    directory = str(tmp_path)
    argv = _entry_points(directory)[entry_point]
    completed = _start(argv, _INTERRUPT_AFTER_THE_RESET, directory)
    assert completed.returncode == -signal.SIGINT, completed.stderr
    assert completed.stderr == b""
    assert completed.stdout == b""


@pytest.mark.parametrize("entry_point", ["module", "console-script", "bin-script"])
def test_an_interrupt_inside_the_reset_is_delivered_again_by_the_signal(
    entry_point: str, tmp_path: object
) -> None:
    import signal

    directory = str(tmp_path)
    argv = _entry_points(directory)[entry_point]
    completed = _start(argv, _INTERRUPT_INSIDE_THE_RESET, directory)
    assert os.path.exists(os.path.join(directory, "raised")), "the reset never asked"
    assert completed.returncode == -signal.SIGINT, completed.stderr
    assert completed.stderr == b""


@pytest.mark.parametrize("entry_point", ["module", "console-script", "bin-script"])
def test_every_entry_point_leaves_an_ignored_sigint_ignored(
    entry_point: str, tmp_path: object
) -> None:
    directory = str(tmp_path)
    argv = _entry_points(directory)[entry_point]
    completed = _start(argv, _INTERRUPT_AFTER_THE_RESET, directory, ignore_sigint=True)
    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.startswith(b"herdr-run ")


def test_importing_the_package_resets_the_interpreter_sigint_handler() -> None:
    """The reset is the package's first statement, so it runs for any importer."""
    probe = (
        "import signal, herdr_run; "
        "print(signal.getsignal(signal.SIGINT) == signal.SIG_DFL, hasattr(herdr_run, '_signal'))"
    )
    completed = subprocess.run(
        [sys.executable, "-c", probe],
        env=_clean_environment(),
        capture_output=True,
        check=False,
        timeout=60,
    )
    assert completed.stdout == b"True False\n", completed.stderr


def test_every_subcommand_that_reads_the_configuration_warns_about_left_out_entries(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A deny list written before a built-in entry existed must not lose that entry silently."""
    root = str(tmp_path)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write('deny_anywhere: ["--upload-pack", "--receive-pack"]\n')
    monkeypatch.chdir(root)
    warning = (
        f"herdr-run: WARNING: {os.path.join(root, '.herdr-run.yaml')} leaves out built-in deny "
        'entries, so they are not enforced: deny_anywhere "--exec". To adopt them, run '
        f"'herdr-run init --upgrade' in {root} or add them by hand.\n"
    )
    for argv, code in (
        (["check", "git push --exec=/tmp/evil origin"], 0),
        (["--agent", "a", "config"], 0),
        (["--agent", "a", "run", "--dry-run", "git status"], 0),
    ):
        assert main(argv) == code, argv
        captured = capsys.readouterr()
        assert captured.err.startswith(warning), (argv, captured.err)
        assert captured.err.count("WARNING: ") == 1 + captured.err.count("NOT git-ignored")

    # Documentation and init read no configuration, so they say nothing about it.
    assert main(["init", "--help"]) == 0
    assert capsys.readouterr().err == ""

"""Readiness-detection tests.

The readiness check decides whether it is safe to type into a terminal a human may also be using,
so each signal is bracketed on both sides: it must FIRE on the state it claims to detect and stay
INERT on the states it does not. A veto that never fires and a veto that always fires are equally
useless, and only a two-sided test tells them apart.
"""

from __future__ import annotations

import pytest

from herdr_run.client import ProcessInfo
from herdr_run.config import Config
from herdr_run.readiness import assess_process, assess_prompt, infer_prompt_tail, parse_ps1, prompt_tail_of


# --- PS1 parsing -------------------------------------------------------------------------------


def test_parses_the_real_bashrc_prompt_from_this_fleet() -> None:
    """The exact PS1 in use on devbig014 — a two-line prompt ending in a colour-reset escape."""
    rc = (
        '# PS1="commented out, must be ignored"\n'
        'PS1="\\[\\033[0;34m\\][\\[\\033[0;31m\\]\\u\\[\\033[0;31m\\]@\\[\\033[0;31m\\]\\h '
        '\\[\\033[0;33m\\]\\w\\[\\033[0;34m\\]] \\[\\033[1;36m\\] \\n\\$ \\[\\033[0m\\]"\n'
    )
    ps1 = parse_ps1(rc)
    assert ps1 is not None
    # The zero-width colour reset must not become part of the visible tail.
    assert prompt_tail_of(ps1) == "$ "


def test_ignores_commented_assignment() -> None:
    assert parse_ps1('# PS1="never"\n') is None


def test_last_assignment_wins() -> None:
    rc = 'PS1="first> "\nPS1="second% "\n'
    assert parse_ps1(rc) == "second% "


@pytest.mark.parametrize(
    "ps1,expected",
    [
        (r"\u@\h \W\$ ", "$ "),
        (r"[\u@\h \w]\$ ", "$ "),
        (r"\w > ", " > "),
        (r"$(git branch) % ", " % "),
        (r"${USER}# ", "# "),
        (r"\u@\h:\w", None),  # ends on an escape: no fixed literal tail to match
        (r"\u ", None),  # tail is whitespace only
    ],
)
def test_prompt_tail_extraction(ps1: str, expected: str | None) -> None:
    assert prompt_tail_of(ps1) == expected


def test_explicit_config_tail_overrides_inference(tmp_path: object) -> None:
    config = Config(prompt_tail="PROMPT> ")
    assert infer_prompt_tail(config, home="/nonexistent") == "PROMPT> "


def test_inference_abstains_when_no_rc_file_is_readable() -> None:
    assert infer_prompt_tail(Config(), home="/nonexistent-home-dir") is None


# --- prompt veto -------------------------------------------------------------------------------


def test_prompt_clean_on_bare_prompt() -> None:
    """`herdr pane read` strips trailing spaces, so an idle "$ " prompt renders as the line "$"."""
    signal = assess_prompt("[newton@devbig014 ~/work/dev-widget]\n$\n", "$ ")
    assert signal.verdict == "clean"


def test_prompt_dirty_on_half_typed_command() -> None:
    """The case the process signal CANNOT see: shell idle, but a human left text on the line."""
    signal = assess_prompt("[newton@host ~]\n$ echo I-WAS-HALF-TYPED\n", "$ ")
    assert signal.verdict == "dirty"
    assert "I-WAS-HALF-TYPED" in signal.reason


def test_prompt_clean_for_single_line_prompt() -> None:
    assert assess_prompt("[newton@host ~]$\n", "$ ").verdict == "clean"


def test_prompt_dirty_for_single_line_prompt_with_text() -> None:
    assert assess_prompt("[newton@host ~]$ git status\n", "$ ").verdict == "dirty"


def test_prompt_abstains_without_a_tail() -> None:
    """No inferable prompt must ABSTAIN, never guess in either direction."""
    signal = assess_prompt("[newton@host ~]$\n", None)
    assert signal.verdict == "abstain"


def test_prompt_abstains_when_tail_absent_from_last_line() -> None:
    assert assess_prompt("Cloning into 'repo'...\n", "$ ").verdict == "abstain"


def test_prompt_abstains_on_empty_pane() -> None:
    assert assess_prompt("\n  \n", "$ ").verdict == "abstain"


def test_prompt_ignores_blank_trailing_lines() -> None:
    assert assess_prompt("[newton@host ~]\n$\n\n\n", "$ ").verdict == "clean"


# --- process signal ----------------------------------------------------------------------------


def _info(*, shell_pid: int, pgid: int, foreground: tuple[tuple[int, str, str], ...]) -> ProcessInfo:
    return ProcessInfo(pane_id="w1:p1", shell_pid=shell_pid, foreground_pgid=pgid, foreground=foreground)


def test_process_idle_when_shell_owns_its_own_foreground_group() -> None:
    signal = assess_process(_info(shell_pid=100, pgid=100, foreground=((100, "bash", "/bin/bash"),)), Config())
    assert signal.idle is True


def test_process_busy_when_a_job_owns_the_foreground_group() -> None:
    signal = assess_process(
        _info(shell_pid=100, pgid=200, foreground=((200, "git", "git push"),)), Config()
    )
    assert signal.idle is False
    assert "git" in signal.reason


def test_process_busy_with_multiple_foreground_processes() -> None:
    """A pipeline: pgid can equal the shell pid transiently, but the members give it away."""
    signal = assess_process(
        _info(shell_pid=100, pgid=100, foreground=((100, "bash", "b"), (101, "git", "g"))), Config()
    )
    assert signal.idle is False


def test_process_busy_when_foreground_process_is_not_the_shell_pid() -> None:
    signal = assess_process(_info(shell_pid=100, pgid=100, foreground=((101, "bash", "b"),)), Config())
    assert signal.idle is False


def test_process_busy_when_pane_runs_a_non_shell() -> None:
    """An editor or agent TUI in the pane is not a shell prompt, whatever its process group says."""
    signal = assess_process(_info(shell_pid=100, pgid=100, foreground=((100, "vim", "vim x"),)), Config())
    assert signal.idle is False
    assert "not a known shell" in signal.reason


def test_process_accepts_other_configured_shells() -> None:
    signal = assess_process(_info(shell_pid=7, pgid=7, foreground=((7, "zsh", "/bin/zsh"),)), Config())
    assert signal.idle is True

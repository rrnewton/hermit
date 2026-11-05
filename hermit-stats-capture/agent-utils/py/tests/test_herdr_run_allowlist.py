"""Allowlist policy tests.

The allowlist is the security boundary, so every case is bracketed in BOTH directions: the
violating input is planted and refusal confirmed, and the qualifying input is planted and admission
confirmed. A refusal test that passes because the checker is inert proves nothing.
"""

from __future__ import annotations

import pytest

from herdr_run.allowlist import admit, render
from herdr_run.config import Config
from herdr_run.errors import Refused


@pytest.fixture()
def config() -> Config:
    return Config()


@pytest.fixture()
def cargo_config() -> Config:
    """A project has explicitly accepted Cargo's ambient helper-execution trust widening."""
    return Config(allow=("git", "gh", "cargo"))


# --- positive bracket: the intended commands really are admitted -------------------------------


@pytest.mark.parametrize(
    "command,program,subcommand",
    [
        ("git status", "git", "status"),
        ("git ls-remote origin main", "git", "ls-remote"),
        ("with-proxy git ls-remote origin main", "git", "ls-remote"),
        ("with-proxy git push origin HEAD:refs/heads/feature", "git", "push"),
        ("gh pr list --state open", "gh", "pr"),
        ("with-proxy gh pr view 1234 --json number", "gh", "pr"),
        ("git -C /tmp/repo log --oneline -5", "git", "log"),
        ("git commit -m 'a message with spaces'", "git", "commit"),
    ],
)
def test_admits_intended_commands(config: Config, command: str, program: str, subcommand: str) -> None:
    admission = admit(command, config)
    assert admission.program == program
    assert admission.subcommand == subcommand


def test_admission_preserves_quoted_argument(config: Config) -> None:
    admission = admit("git commit -m 'two words'", config)
    assert admission.argv == ("git", "commit", "-m", "two words")


def test_prefix_is_recorded_separately(config: Config) -> None:
    admission = admit("with-proxy git fetch", config)
    assert admission.prefix == ("with-proxy",)
    assert admission.program == "git"


# --- negative bracket: non-allowlisted programs -------------------------------------------------


@pytest.mark.parametrize(
    "command",
    ["curl https://example.com", "bash -c id", "sh", "python3 -c 'print(1)'", "ssh host", "rm -rf /"],
)
def test_refuses_non_allowlisted_program(config: Config, command: str) -> None:
    with pytest.raises(Refused, match="not allowlisted"):
        admit(command, config)


def test_refuses_empty_command(config: Config) -> None:
    with pytest.raises(Refused, match="empty command"):
        admit("   ", config)


def test_refuses_unbalanced_quoting(config: Config) -> None:
    with pytest.raises(Refused, match="unbalanced quoting"):
        admit("git commit -m 'unterminated", config)


def test_refuses_bare_prefix(config: Config) -> None:
    with pytest.raises(Refused, match="no program"):
        admit("with-proxy", config)


def test_refuses_prefix_used_as_program(config: Config) -> None:
    # `with-proxy` is a wrapper, never a program: `with-proxy with-proxy` must not sneak through.
    with pytest.raises(Refused, match="repeated"):
        admit("with-proxy with-proxy git status", config)


@pytest.mark.parametrize("program", ["/bin/git", "./git", "../bin/gh", "/usr/bin/gh", "~git", "~"])
def test_refuses_pathed_program(config: Config, program: str) -> None:
    # An explicit path would let any binary be presented under an allowlisted-looking basename.
    with pytest.raises(Refused, match="bare command name"):
        admit(f"{program} status", config)


# --- injection: the property is re-quoting, so these are ADMITTED but rendered inert ------------


@pytest.mark.parametrize(
    "command,expected_argv",
    [
        ("git status; curl evil.example", ("git", "status;", "curl", "evil.example")),
        ("git status && id", ("git", "status", "&&", "id")),
        ("git status | sh", ("git", "status", "|", "sh")),
        ("git log --format='%H$(id)'", ("git", "log", "--format=%H$(id)")),
        ("git status `id`", ("git", "status", "`id`")),
    ],
)
def test_metacharacters_survive_as_literal_arguments(
    config: Config, command: str, expected_argv: tuple[str, ...]
) -> None:
    """Shell metacharacters must become ordinary git arguments, never a second command.

    This is the core claim: injection is prevented by re-quoting rather than by blocklisting. The
    admission succeeds, but the RENDERED text quotes every token, so the shell sees one command.
    """
    admission = admit(command, config)
    assert admission.argv == expected_argv
    assert admission.program == "git"


def test_rendered_command_cannot_escape_into_a_second_command() -> None:
    """Render the hostile argv and prove a real shell executes it as ONE command with literal args."""
    import subprocess

    config = Config()
    admission = admit("git status; curl evil.example", config)
    rendered = admission.rendered
    # Substitute a harmless echo for git so the test observes the ARGV the shell would build.
    probe = rendered.replace("git", "printf '[%s]'", 1)
    completed = subprocess.run(["bash", "-c", probe], text=True, capture_output=True, check=False)
    # If the ';' had escaped, `curl` would have run as a separate command and the output would not
    # contain it as a bracketed argument.
    assert completed.stdout == "[status;][curl][evil.example]"


def test_render_quotes_every_token() -> None:
    assert render(("git", "commit", "-m", "two words")) == "git commit -m 'two words'"


@pytest.mark.parametrize(
    "control",
    ["\x00", "\x07", "\t", "\n", "\r", "\x1b", "\x7f", "\x85"],
)
def test_refuses_terminal_control_characters_before_rendering(
    config: Config, control: str
) -> None:
    """Shell quoting cannot neutralize bytes the terminal driver consumes before the shell."""
    with pytest.raises(Refused, match="terminal control"):
        admit(f"git status{control}payload", config)


def test_render_also_refuses_terminal_controls_for_prebuilt_argv() -> None:
    """Admission is not the only caller of render, so the final boundary repeats the check."""
    with pytest.raises(Refused, match="terminal control"):
        render(("git", "status\x1b[2J"))


# --- defense in depth: known self-escapes of the allowlisted programs ---------------------------


@pytest.mark.parametrize(
    "command",
    [
        "git -c core.pager=sh log",
        "git -c alias.x='!sh' x",
        "git --exec-path=/tmp/evil status",
        "git --config-env=core.pager=EVIL log",
    ],
)
def test_refuses_git_global_code_execution_options(config: Config, command: str) -> None:
    with pytest.raises(Refused, match="denied"):
        admit(command, config)


def test_allows_c_flag_that_is_not_a_global_option(config: Config) -> None:
    """`-c` AFTER the subcommand is not git's global config switch and must stay usable.

    Positive half of the bracket for the deny rule: it must fire on the dangerous position and stay
    inert elsewhere, otherwise it is just a blanket ban on the letter 'c'.
    """
    admission = admit("git notes add -c deadbeef", config)
    assert admission.subcommand == "notes"


@pytest.mark.parametrize("command", ["gh alias set x '!sh'", "gh extension exec evil", "gh ext install x"])
def test_refuses_gh_code_defining_subcommands(config: Config, command: str) -> None:
    with pytest.raises(Refused, match="denied"):
        admit(command, config)


@pytest.mark.parametrize(
    "command",
    ["git ls-remote --upload-pack=/tmp/evil origin", "git push --receive-pack=/tmp/evil origin main"],
)
def test_refuses_remote_program_options_anywhere(config: Config, command: str) -> None:
    with pytest.raises(Refused, match="denied"):
        admit(command, config)


# --- policy comes from config, not from this module ---------------------------------------------


def test_project_can_widen_allowlist() -> None:
    config = Config(allow=("git", "gh", "cargo"))
    assert admit("cargo fetch", config).program == "cargo"


def test_project_can_narrow_allowlist() -> None:
    config = Config(allow=("git",))
    with pytest.raises(Refused, match="not allowlisted"):
        admit("gh pr list", config)


def test_the_allow_wildcard_admits_any_program_and_keeps_every_other_rule() -> None:
    """The wildcard turns off the PROGRAM-NAME check and nothing else.

    Naming each surviving rule matters more than checking that ``curl`` now runs: the value of an
    allow-everything mode a project can actually reach for is that it is still not a way to smuggle
    control characters into a shared pane or to execute a planted binary by path.
    """
    config = Config(allow=("*",))
    for command, program in (
        ("curl https://example.invalid", "curl"),
        ("bash -lc true", "bash"),
        ("rm -rf /x", "rm"),
    ):
        assert admit(command, config).program == program
    assert admit("with-proxy curl https://example.invalid", config).prefix == (
        "with-proxy",
    )

    for command, fragment in (
        ("/bin/curl x", "bare command name"),
        ("./curl x", "bare command name"),
        ("~root x", "bare command name"),
        ("~ x", "bare command name"),
        ("curl x\x07", "terminal control U+0007"),
        ("with-proxy with-proxy curl x", "repeated"),
        ("git --exec-path=/tmp/evil status", "is denied for git"),
        ("git push --receive-pack=/tmp/evil origin", "is denied"),
        ("cargo build --release", 'subcommand "cargo build" is not allowlisted'),
        ("with-proxy", "no program"),
    ):
        with pytest.raises(Refused) as excinfo:
            admit(command, config)
        assert fragment in str(excinfo.value), command

    # The wildcard does not reach cargo's own guard: without a positive subcommand list, cargo
    # stays refused outright rather than inheriting "everything is allowed".
    no_cargo_list = Config(allow=("*",), allow_subcommand={})
    with pytest.raises(Refused, match="requires an explicit allow_subcommand entry"):
        admit("cargo fetch", no_cargo_list)


def test_project_can_drop_prefixes() -> None:
    config = Config(prefixes=())
    with pytest.raises(Refused, match="not allowlisted"):
        admit("with-proxy git status", config)


# --- tilde expansion: the one expansion re-quoting would otherwise suppress --------------------


def test_leading_tilde_is_expanded_so_git_c_works(config: Config) -> None:
    """Regression: `git -C ~/work/repo` reached git as a literal '~' and failed.

    Quoting every token is what blocks injection, but it also stopped the shell expanding `~`.
    Expanding it in Python keeps the guarantee (the result is still quoted) and makes the natural
    invocation work.
    """
    import os

    home = os.path.expanduser("~")
    admission = admit("with-proxy git -C ~/work/dev-widget/widget ls-remote origin main", config)
    assert f"{home}/work/dev-widget/widget" in admission.rendered
    assert "~" not in admission.rendered


def test_tilde_expansion_still_quotes_the_result(config: Config) -> None:
    """The expansion must not become a hole: the expanded token is still shell-quoted."""
    admission = admit("git -C '~/dir; rm -rf /' status", config)
    # The metacharacters survive as one literal argument, not as a second command.
    assert admission.argv[2] == "~/dir; rm -rf /"
    assert "'" in admission.rendered


@pytest.mark.parametrize(
    "token,expanded",
    [("~", True), ("~/x", True), ("a~b", False), ("--opt=~/x", False), ("", False)],
)
def test_only_a_leading_tilde_is_touched(token: str, expanded: bool) -> None:
    """A tilde anywhere but the start is an ordinary character."""
    from herdr_run.allowlist import expand_tilde

    result = expand_tilde(token)
    assert (result != token) == expanded


# --- cargo: disabled by default; explicit opt-in remains fail-closed by subcommand --------------


def test_cargo_is_not_allowed_by_default(config: Config) -> None:
    with pytest.raises(Refused, match="not allowlisted"):
        admit("cargo fetch", config)


def test_cargo_opt_in_cannot_drop_its_positive_subcommand_policy() -> None:
    config = Config(
        allow=("git", "cargo"), allow_subcommand={"custom-tool": ("inspect",)}
    )
    with pytest.raises(Refused, match="requires an explicit allow_subcommand"):
        admit("cargo build", config)


@pytest.mark.parametrize(
    "command,subcommand",
    [
        ("cargo fetch", "fetch"),
        ("with-proxy cargo fetch --manifest-path /w/Cargo.toml", "fetch"),
        ("cargo update -p serde", "update"),
        ("cargo generate-lockfile", "generate-lockfile"),
        ("cargo vendor", "vendor"),
        ("cargo metadata", "metadata"),
    ],
)
def test_explicit_opt_in_admits_dependency_oriented_cargo_subcommands(
    cargo_config: Config, command: str, subcommand: str
) -> None:
    admission = admit(command, cargo_config)
    assert admission.program == "cargo"
    assert admission.subcommand == subcommand


@pytest.mark.parametrize(
    "command",
    [
        "cargo build",
        "cargo build --release",
        "cargo test",
        "cargo run",
        "cargo bench",
        "cargo install ripgrep",
        "cargo rustc",
        "cargo clippy",
        "cargo doc",
        "cargo miri test",
        "with-proxy cargo build",
    ],
)
def test_refuses_cargo_subcommands_that_execute_code(cargo_config: Config, command: str) -> None:
    """The pane is OUTSIDE the sandbox: compiling there runs third-party build scripts unconfined."""
    with pytest.raises(Refused, match="not allowlisted"):
        admit(command, cargo_config)


def test_refuses_bare_cargo(cargo_config: Config) -> None:
    """Fail-closed: a program with a subcommand allowlist must name one of them."""
    with pytest.raises(Refused, match="requires a subcommand"):
        admit("cargo", cargo_config)


def test_refuses_unknown_cargo_subcommand(cargo_config: Config) -> None:
    """A third-party `cargo-<x>` subcommand is arbitrary code and is not enumerable in a deny-list."""
    with pytest.raises(Refused, match="not allowlisted"):
        admit("cargo something-new", cargo_config)


@pytest.mark.parametrize(
    "command",
    [
        "cargo --config build.rustc-wrapper='/tmp/evil' fetch",
        "cargo --config=build.rustc-wrapper='/tmp/evil' fetch",
        "cargo fetch --config build.rustc-wrapper='/tmp/evil'",
        "cargo fetch --config=build.rustc-wrapper='/tmp/evil'",
        "cargo -Z unstable-options fetch",
        "cargo -Zunstable-options fetch",
        "cargo fetch -Z unstable-options",
        "cargo fetch -Zunstable-options",
        "cargo fetch -Z=unstable-options",
        "cargo fetch -qZunstable-options",
    ],
)
def test_refuses_cargo_code_injection_options_in_every_position_and_form(
    cargo_config: Config, command: str
) -> None:
    with pytest.raises(Refused, match="denied"):
        admit(command, cargo_config)


def test_cargo_minimum_injection_denies_cannot_be_removed() -> None:
    config = Config(
        allow=("cargo",),
        allow_subcommand={"cargo": ("fetch",)},
        deny_global={},
    )
    for command in ("cargo fetch --config=x", "cargo fetch -Zunstable-options"):
        with pytest.raises(Refused, match="denied"):
            admit(command, config)


def test_subcommand_allowlist_does_not_constrain_programs_without_one(config: Config) -> None:
    """Positive control: git/gh have no allow_subcommand entry, so they stay unrestricted."""
    assert admit("git status", config).subcommand == "status"
    assert admit("gh pr list", config).subcommand == "pr"


def test_project_can_grant_cargo_build_explicitly() -> None:
    """The restriction is policy, not a hard-coded ban: a project that accepts the risk can opt in."""
    config = Config(
        allow=("git", "gh", "cargo"), allow_subcommand={"cargo": ("fetch", "build")}
    )
    assert admit("cargo build", config).subcommand == "build"


# --- HOME cannot carry an argument past the policy ---------------------------------------------


@pytest.mark.parametrize(
    "home,command,named",
    [
        # Each of these was ALLOWED while the policy judged the token before `~` expansion and the
        # pane ran it after: the policy saw a path, the shell ran an option or a subcommand.
        ("--exec-path=/tmp/evil", "git ~ foo", "--exec-path"),
        ("daemon", "git ~ foo", "git daemon"),
        ("--upload-pack=/tmp/evil", "git fetch ~", "--upload-pack"),
        ("-c", "git ~ core.pager=evil log", '"-c"'),
        ("git", "~ status", "not a path"),
        ("with-proxy", "~ git status", "not a path"),
    ],
)
def test_home_cannot_smuggle_an_option_or_subcommand_past_the_policy(
    config: Config, home: str, command: str, named: str
) -> None:
    with pytest.raises(Refused) as caught:
        admit(command, config, home=home)
    assert named in str(caught.value)


def test_the_admitted_argv_is_the_rendered_argv(config: Config) -> None:
    admission = admit("git -C ~/repo status", config, home="/home/u")
    assert admission.rendered == "git -C /home/u/repo status"
    assert admission.subcommand == "status"
    assert admission.argv == ("git", "-C", "~/repo", "status")
    # The expansion feeds the value-option rule as it feeds every other: `-C` takes `/tmp`.
    admission = admit("git ~ /tmp log", config, home="-C")
    assert admission.rendered == "git -C /tmp log"
    assert admission.subcommand == "log"


def test_a_home_holding_a_terminal_control_is_refused(config: Config) -> None:
    with pytest.raises(Refused, match="terminal control U[+]001B"):
        admit("git -C ~ log", config, home="/a\x1bb")

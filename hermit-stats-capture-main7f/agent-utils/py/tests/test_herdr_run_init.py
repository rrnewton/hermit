"""`herdr-run init`: the generated `.herdr-run.yaml` is the configuration reference.

The guide points AT this file instead of restating the knobs, so the file has to actually carry
all of them, and adopting it has to change nothing until somebody edits it. Both are asserted here
rather than left to review.
"""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path

import pytest

from herdr_run.cli import main
from herdr_run.config import CONFIG_FILENAMES, KNOWN_KEYS, Config, load_config, parse_config
from herdr_run.errors import EXIT_CONFIG, ConfigError
from herdr_run.init import (
    UPGRADES,
    config_template,
    upgrade_config_file,
    upgrade_text,
    write_config_template,
)
from herdr_run.yamlcore import core_load

#: Configurations earlier releases wrote, byte for byte; the shared corpus describes each one.
_EARLIER = Path(__file__).resolve().parents[2] / "rs" / "herdr-run" / "testdata" / "earlier_configs"


def test_the_template_carries_every_configuration_key() -> None:
    """The promise that lets the guide point at the generated file instead of restating it.

    A new configuration key that nobody adds to the template turns the file into a partial answer,
    and a partial answer is worse than none because it looks complete.
    """
    template = config_template()
    for key in KNOWN_KEYS:
        assert f"\n{key}:" in template, f"the generated .herdr-run.yaml never sets {key}"


def test_the_written_template_parses_to_the_built_in_defaults(tmp_path: object) -> None:
    """Adopting the generated file must change nothing until somebody edits it."""
    pytest.importorskip("yaml")
    root = str(tmp_path)
    path = write_config_template(root, force=False)
    assert path == os.path.join(root, ".herdr-run.yaml")
    written = load_config(explicit_path=path, start_dir=root)
    expected = Config(source_path=written.source_path, project_root=written.project_root)
    assert written == expected


def test_the_template_names_the_allow_everything_mode_and_the_human_only_rule() -> None:
    """The two things the template has to say out loud."""
    template = config_template()
    assert 'allow: ["*"]' in template
    assert "ALLOW-EVERYTHING MODE" in template
    assert "a human-only knob" in template
    assert "DO NOT LET AN AGENT EDIT THIS SECTION" in template
    assert "worktrees/slotNN/" in template


@pytest.mark.parametrize("filename", CONFIG_FILENAMES)
def test_an_existing_configuration_is_not_clobbered_without_force(
    filename: str, tmp_path: object
) -> None:
    root = str(tmp_path)
    existing = os.path.join(root, filename)
    with open(existing, "w", encoding="utf-8") as handle:
        handle.write("workspace: mine\n")

    with pytest.raises(ConfigError) as excinfo:
        write_config_template(root, force=False)
    assert excinfo.value.exit_code == EXIT_CONFIG
    assert "pass --force" in str(excinfo.value)
    with open(existing, encoding="utf-8") as handle:
        assert handle.read() == "workspace: mine\n", f"{filename} was modified by a refused init"

    if filename != ".herdr-run.yaml":
        # Writing the .yaml would silently take precedence over the .yml that is already there;
        # two files and no warning is a puzzle, not a config.
        assert "takes precedence over it" in str(excinfo.value)
        assert not os.path.exists(os.path.join(root, ".herdr-run.yaml"))


def test_force_overwrites_an_existing_configuration(tmp_path: object) -> None:
    root = str(tmp_path)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write("workspace: mine\n")
    path = write_config_template(root, force=True)
    with open(path, encoding="utf-8") as handle:
        assert handle.read() == config_template()


def test_init_writes_and_then_refuses_through_the_cli(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = str(tmp_path)
    monkeypatch.chdir(root)

    assert main(["init"]) == 0
    captured = capsys.readouterr()
    assert captured.out.startswith(f"wrote {os.path.join(root, '.herdr-run.yaml')}\n")
    assert "HUMAN-ONLY knob" in captured.out
    assert captured.err == ""

    assert main(["init"]) == EXIT_CONFIG
    captured = capsys.readouterr()
    assert captured.out == ""
    assert "--force" in captured.err

    assert main(["--json", "init", "--force"]) == 0
    document = json.loads(capsys.readouterr().out)
    assert document == {"created": True, "path": os.path.join(root, ".herdr-run.yaml")}


def test_init_writes_even_when_the_discovered_configuration_is_broken(
    tmp_path: object, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """The reason to reach for `init` is often that what is already there does not parse.

    Refusing to write a fresh template because the old one is broken would be exactly the wrong
    moment to be strict, so `init` runs before any configuration is loaded.
    """
    root = str(tmp_path)
    nested = os.path.join(root, "slot")
    os.makedirs(nested)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write("allow: [\n")
    monkeypatch.chdir(nested)

    assert main(["init"]) == 0
    assert capsys.readouterr().err == ""
    assert os.path.isfile(os.path.join(nested, ".herdr-run.yaml"))


# --- init --upgrade ------------------------------------------------------------------------------


def test_every_upgrade_only_adds_the_entries_it_names() -> None:
    """Each rewrite must say exactly what it adds, and take nothing away."""
    for upgrade in UPGRADES:
        top, _, program = upgrade.setting.partition(".")

        def entries(text: str) -> list[str]:
            document = core_load(text)
            assert isinstance(document, dict)
            value = document[top]
            if program:
                assert isinstance(value, dict)
                value = value[program]
            assert isinstance(value, list)
            return [str(entry) for entry in value]

        earlier, current = entries(upgrade.earlier), entries(upgrade.current)
        assert current[: len(earlier)] == earlier, upgrade.setting
        assert tuple(current[len(earlier) :]) == upgrade.added, upgrade.setting
        # What the rewrite produces is what this release's init writes for the same setting.
        assert upgrade.current in config_template(), upgrade.setting


@pytest.mark.parametrize("name", sorted(path.name for path in _EARLIER.iterdir()))
def test_init_upgrade_brings_every_earlier_file_up_to_date(
    name: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    original = (_EARLIER / name).read_text(encoding="utf-8")
    target = tmp_path / ".herdr-run.yaml"
    target.write_text(original, encoding="utf-8")
    target.chmod(0o640)
    monkeypatch.chdir(tmp_path)

    # Before: the file loads, says what it leaves out, and leaves `--exec` admitted.
    assert main(["check", "git push --exec=/tmp/evil origin"]) == 0
    captured = capsys.readouterr()
    assert captured.err.startswith(
        f"herdr-run: WARNING: {target} leaves out built-in deny entries, so they are not "
        'enforced: deny_anywhere "--exec". To adopt them, run '
        f"'herdr-run init --upgrade' in {tmp_path} or add them by hand.\n"
    )

    assert main(["init", "--upgrade"]) == 0
    captured = capsys.readouterr()
    assert captured.err == ""
    assert captured.out.startswith(f"upgraded {target}\n  deny_anywhere: added \"--exec\"\n")
    assert captured.out.endswith("Every built-in deny entry is in force.\n")
    upgraded = target.read_text(encoding="utf-8")
    assert upgraded == upgrade_text(original)[0]
    # Written in place: the mode is the file's own, not a fresh file's.
    assert stat.S_IMODE(target.stat().st_mode) == 0o640

    # After: nothing to warn about, and `--exec` refused.
    assert main(["check", "git push --exec=/tmp/evil origin"]) == 77
    assert "WARNING" not in capsys.readouterr().err

    # A second upgrade finds nothing to do and writes nothing.
    before = target.stat().st_mtime_ns
    assert main(["init", "--upgrade"]) == 0
    assert capsys.readouterr().out == (
        f"nothing to rewrite in {target}\nEvery built-in deny entry is in force.\n"
    )
    assert target.stat().st_mtime_ns == before
    assert target.read_text(encoding="utf-8") == upgraded


def test_init_upgrade_leaves_an_edited_list_alone_and_says_what_it_lacks(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    original = (_EARLIER / "0.3.0-init-93b44b35.yaml").read_text(encoding="utf-8")
    edited = original.replace(
        'deny_anywhere: ["--upload-pack", "--receive-pack"]',
        'deny_anywhere: ["--receive-pack", "--upload-pack"]',
    ).replace('git: ["-c", "--config-env", "--exec-path", "--namespace"]', 'git: ["-c"]')
    (tmp_path / ".herdr-run.yaml").write_text(edited, encoding="utf-8")
    monkeypatch.chdir(tmp_path)

    assert main(["--json", "init", "--upgrade"]) == 0
    document = json.loads(capsys.readouterr().out)
    assert document == {
        "missing": [
            {
                "entries": ["--config-env", "--exec-path", "--namespace"],
                "setting": "deny_global.git",
            },
            {"entries": ["--exec"], "setting": "deny_anywhere"},
        ],
        "path": str(tmp_path / ".herdr-run.yaml"),
        "upgraded": [
            {
                "added": ["--super-prefix", "--shallow-file", "--attr-source"],
                "setting": "value_options.git",
            }
        ],
    }
    text = (tmp_path / ".herdr-run.yaml").read_text(encoding="utf-8")
    assert 'deny_anywhere: ["--receive-pack", "--upload-pack"]\n' in text
    assert '"--super-prefix", "--shallow-file", "--attr-source"]' in text

    assert main(["init", "--upgrade"]) == 0
    assert capsys.readouterr().out == (
        f"nothing to rewrite in {tmp_path / '.herdr-run.yaml'}\n"
        "Still left out, from lists that are not as an earlier herdr-run wrote them; add them by "
        "hand to adopt them:\n"
        '  deny_global.git: "--config-env", "--exec-path", "--namespace"\n'
        '  deny_anywhere: "--exec"\n'
    )


def test_upgrade_text_matches_whole_lines_only() -> None:
    line = UPGRADES[0].earlier
    assert upgrade_text("# " + line)[1] == ()
    assert upgrade_text("x" + line)[1] == ()
    assert upgrade_text(line.replace("\n", "\r\n"))[1] == ()
    # A last line without its line feed is still that setting, and still has no line feed after.
    upgraded, applied = upgrade_text("workspace: w\n" + line.rstrip("\n"))
    assert applied == (UPGRADES[0],)
    assert upgraded == "workspace: w\n" + UPGRADES[0].current.rstrip("\n")


def test_init_upgrade_works_on_a_yml_and_refuses_when_there_is_no_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.chdir(tmp_path)
    assert main(["init", "--upgrade"]) == EXIT_CONFIG
    captured = capsys.readouterr()
    assert captured.out == ""
    assert captured.err == (
        "herdr-run: there is no .herdr-run.yaml or .herdr-run.yml in "
        f"{tmp_path} to upgrade; run 'herdr-run init' to write one\n"
    )
    assert not (tmp_path / ".herdr-run.yaml").exists()

    yml = tmp_path / ".herdr-run.yml"
    yml.write_text(UPGRADES[0].earlier, encoding="utf-8")
    result = upgrade_config_file(str(tmp_path))
    assert result.path == str(yml)
    assert yml.read_text(encoding="utf-8") == UPGRADES[0].current
    assert not (tmp_path / ".herdr-run.yaml").exists()


def test_init_upgrade_refuses_a_file_that_does_not_load(tmp_path: Path) -> None:
    path = tmp_path / ".herdr-run.yaml"
    path.write_text(UPGRADES[0].earlier + "allowlist: [git]\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="unknown key"):
        upgrade_config_file(str(tmp_path))
    assert path.read_text(encoding="utf-8") == UPGRADES[0].earlier + "allowlist: [git]\n"


def test_init_upgrade_writes_nothing_when_the_rewrite_would_read_differently(
    tmp_path: Path,
) -> None:
    """The earlier lines can appear inside another setting; rewriting them there is refused."""
    text = "deny_global: {git: [-c],\n" + UPGRADES[0].earlier + "}\n"
    loaded = parse_config(core_load(text), source_path=None, project_root="/")
    assert "deny_anywhere" in loaded.deny_global
    assert upgrade_text(text)[1] == (UPGRADES[0],)
    path = tmp_path / ".herdr-run.yaml"
    path.write_text(text, encoding="utf-8")
    with pytest.raises(ConfigError, match="cannot upgrade it automatically") as caught:
        upgrade_config_file(str(tmp_path))
    assert "nothing was written" in str(caught.value)
    assert path.read_text(encoding="utf-8") == text


def test_init_upgrade_keeps_a_symbolic_link(tmp_path: Path) -> None:
    real = tmp_path / "policy.yaml"
    real.write_text(UPGRADES[0].earlier, encoding="utf-8")
    (tmp_path / ".herdr-run.yaml").symlink_to("policy.yaml")
    upgrade_config_file(str(tmp_path))
    assert (tmp_path / ".herdr-run.yaml").is_symlink()
    assert real.read_text(encoding="utf-8") == UPGRADES[0].current


def test_force_and_upgrade_cannot_be_combined(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.chdir(tmp_path)
    for argv in (["init", "--force", "--upgrade"], ["init", "--upgrade", "--force"]):
        assert main(argv) == 2
        captured = capsys.readouterr()
        assert captured.out == ""
        assert captured.err.endswith(
            "herdr-run: error: init: --force and --upgrade cannot be combined: --force writes a "
            "fresh file over the existing one, and --upgrade edits the existing one in place\n"
        )
    assert not (tmp_path / ".herdr-run.yaml").exists()

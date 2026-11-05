"""The shared golden corpora that pin the two herdr-run editions to one behaviour.

Every file under ``rs/herdr-run/testdata/`` is loaded here and by the Rust unit tests, so a rule
that changes in one edition fails the other edition's suite until both agree again:

- ``text_cases.json``: the quoting rule, numeric text, the exit-marker grammar, float text,
  run IDs, and the lines ``check`` prints for an admitted command;
- ``name_verdicts.json``: the name rule's verdict on every code point, U+0000..U+10FFFF, written
  by this module (run it with ``--write-name-verdicts``) and checked code point by code point by
  both suites;
- ``yaml_cases.json``: the configuration YAML subset, accepted values and positioned refusals;
- ``readiness_cases.json``: prompt verdicts and prompt tails inferred from rc files;
- ``protocol_cases.json``: Herdr protocol calls through both brokers, their argv, and their
  values or errors, including server start-up;
- ``record_cases.json``: reap scope, evidence folded from run records, session-cache loading
  and the exact bytes a cache store writes;
- ``child_cases.json``: what every process either edition starts is handed: its environment,
  built by the shared rule, and which signals the caller left ignored stay ignored in it;
- ``policy_cases.json``: configuration loading and command admission, including ``~`` expanded
  before the policy judges an argument, names refused for holding an invisible character or any
  other character a name may not contain, the
  warning a deny list that leaves out a built-in entry draws, and every configuration an earlier
  release wrote: what it admits, what it is warned about, and what ``init --upgrade`` makes of it.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import unicodedata
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import cast

import pytest

from herdr_run import __version__
from herdr_run.allowlist import admit
from herdr_run.cli import _Globals, _resolve_agent, check_text
from herdr_run.client import HerdrClient, child_environment
from herdr_run.config import (
    Config,
    missing_defaults_warning,
    parse_config,
    parse_config_text,
)
from herdr_run.errors import ConfigError, HerdrRunError, Refused
from herdr_run.init import UPGRADES, config_template, upgrade_text
from herdr_run.readiness import assess_prompt, infer_prompt_tail
from herdr_run.reap import evidence_from_runs
from herdr_run.runner import make_run_id
from herdr_run.session import Target, _load_cache, _store_cache
from herdr_run.sweep import _scope_of
from herdr_run.textfmt import (
    FORMAT_CHARACTER_RANGES,
    JSON_MAX_DEPTH,
    UNICODE_VERSION,
    display,
    is_invisible,
    is_noncharacter,
    is_private_use,
    is_unassigned,
    json_text,
    name_violation,
    number_text,
    parse_exit_marker,
    quote,
    strict_json_loads,
)
from herdr_run.yamlcore import YamlError, core_load

_REPOSITORY = Path(__file__).resolve().parents[2]
_TESTDATA = _REPOSITORY / "rs" / "herdr-run" / "testdata"


def _load(name: str) -> dict[str, object]:
    return cast(dict[str, object], json.loads((_TESTDATA / name).read_text(encoding="utf-8")))


TEXT = _load("text_cases.json")
YAML = _load("yaml_cases.json")
READINESS = _load("readiness_cases.json")
PROTOCOL = _load("protocol_cases.json")
RECORDS = _load("record_cases.json")
POLICY = _load("policy_cases.json")
CHILD = _load("child_cases.json")


def _rows(document: dict[str, object], key: str) -> list[list[object]]:
    return cast(list[list[object]], document[key])


# --- text rules ----------------------------------------------------------------------------------


@pytest.mark.parametrize("row", _rows(TEXT, "quote"), ids=repr)
def test_quote(row: list[object]) -> None:
    value, expected = row
    assert quote(cast(str, value)) == expected


@pytest.mark.parametrize("row", _rows(TEXT, "number_text"), ids=repr)
def test_number_text(row: list[object]) -> None:
    value, expected = row
    assert number_text(cast(float, value)) == expected


@pytest.mark.parametrize("row", _rows(TEXT, "exit_marker"), ids=repr)
def test_exit_marker(row: list[object]) -> None:
    payload, expected = row
    assert parse_exit_marker(bytes(cast(list[int], payload))) == expected


@pytest.mark.parametrize("row", _rows(TEXT, "float_repr"), ids=repr)
def test_float_text(row: list[object]) -> None:
    value, expected = row
    assert repr(cast(float, value)) == expected


@pytest.mark.parametrize("row", _rows(TEXT, "display"), ids=ascii)
def test_display(row: list[object]) -> None:
    value, expected = row
    assert display(cast(str, value)) == expected


@pytest.mark.parametrize("row", _rows(TEXT, "name_violation"), ids=ascii)
def test_name_violation(row: list[object]) -> None:
    value, expected = row
    assert name_violation(cast(str, value)) == expected



@pytest.mark.parametrize(
    "code,noncharacter,private_use,unassigned",
    [
        (0xFDCF, False, False, False),
        (0xFDD0, True, False, True),
        (0xFDEF, True, False, True),
        (0xFDF0, False, False, False),
        (0xFFFD, False, False, False),
        (0xFFFE, True, False, True),
        (0x1FFFE, True, False, True),
        (0xE000, False, True, False),
        (0xF8FF, False, True, False),
        (0xF900, False, False, False),
        (0xEFFFD, False, False, True),
        (0xF0000, False, True, False),
        (0xFFFFD, False, True, False),
        (0xFFFFE, True, False, True),
        (0x100000, False, True, False),
        (0x10FFFD, False, True, False),
        (0x10FFFF, True, False, True),
        (0x0377, False, False, False),
        (0x0378, False, False, True),
        (0x1C89, False, False, False),
    ],
    ids=lambda value: str(value) if isinstance(value, bool) else f"U+{value:04X}",
)
def test_the_name_classes_at_their_edges(
    code: int, noncharacter: bool, private_use: bool, unassigned: bool
) -> None:
    character = chr(code)
    assert is_noncharacter(character) == noncharacter
    assert is_private_use(character) == private_use
    assert is_unassigned(character) == unassigned


def test_the_blank_characters_are_invisible_and_the_most_specific_description_wins() -> None:
    # The two assigned characters that draw nothing are invisible, so JSON escapes them too.
    assert is_invisible("\U0001d159") and is_invisible("\U00016fe4")
    assert not is_invisible("\U0001d158") and not is_invisible("\U00016fe3")
    assert json_text(["\U0001d159\ue000"]) == '["\\ud834\\udd59\ue000"]'
    # U+FDD0 and U+E0000 are unassigned too.
    assert name_violation("\ufdd0") == "noncharacter U+FDD0"
    assert name_violation("\U000e0000") == "invisible character U+E0000"

_NAME_VERDICTS = "name_verdicts.json"

_NAME_VERDICTS_ABOUT = (
    "The name rule both editions share (name_violation in textfmt.py / textfmt.rs), on every code "
    "point: runs [first, last, what] in hexadecimal, in order, covering U+0000..U+10FFFF, where "
    "what is the description name_violation gives a name made of that one character, less its "
    "' U+XXXX', or null when that name is accepted. Only the Python edition can hold a surrogate; "
    "the Rust edition checks that it cannot. Written from the Python edition by "
    "'PYTHONPATH=py python3 py/tests/test_herdr_run_golden.py --write-name-verdicts'; both "
    "unit-test suites check every code point against it."
)


def _name_verdict(code: int) -> str | None:
    """What :func:`name_violation` says of a name that is the one character ``code``, less where."""
    violation = name_violation(chr(code))
    if violation is None:
        return None
    what, _, where = violation.rpartition(" ")
    assert where == f"U+{code:04X}", violation
    return what


def _name_verdict_runs() -> list[list[str | None]]:
    """Every code point's verdict, as maximal runs ``[first, last, what]`` in hexadecimal."""
    runs: list[tuple[int, int, str | None]] = []
    for code in range(0x110000):
        what = _name_verdict(code)
        if runs and runs[-1][2] == what:
            runs[-1] = (runs[-1][0], code, what)
        else:
            runs.append((code, code, what))
    return [[f"{first:04X}", f"{last:04X}", what] for first, last, what in runs]


def _name_verdicts_text(runs: list[list[str | None]]) -> str:
    """The corpus file's exact text: one run per line, so a change reads as a short diff."""
    rows = ",\n".join(f"    {json.dumps(run)}" for run in runs)
    return (
        "{\n"
        f'  "_about": {json.dumps(_NAME_VERDICTS_ABOUT)},\n'
        f'  "unicode_version": {json.dumps(UNICODE_VERSION)},\n'
        f'  "verdicts": [\n{rows}\n  ]\n'
        "}\n"
    )


def write_name_verdicts() -> None:
    """Write ``name_verdicts.json`` from this edition's rule."""
    (_TESTDATA / _NAME_VERDICTS).write_text(
        _name_verdicts_text(_name_verdict_runs()), encoding="utf-8"
    )


def test_every_code_point_has_the_shared_name_verdict() -> None:
    """Each of the 1,114,112 code points gets the verdict the corpus, and so the other edition,
    gives it; the file is exactly what :func:`write_name_verdicts` writes."""
    text = (_TESTDATA / _NAME_VERDICTS).read_text(encoding="utf-8")
    document = cast(dict[str, object], json.loads(text))
    assert document["unicode_version"] == UNICODE_VERSION
    expected = cast(list[list[str | None]], document["verdicts"])
    actual = _name_verdict_runs()
    for index, (have, want) in enumerate(zip(actual, expected)):
        assert have == want, f"run {index}: this edition says {have}, the corpus {want}"
    assert len(actual) == len(expected)
    assert text == _name_verdicts_text(actual)


def _version(text: str) -> tuple[int, ...]:
    return tuple(int(part) for part in text.split("."))


def test_the_written_out_tables_agree_with_this_interpreters_unicode() -> None:
    """The tables are written out, not borrowed, but this interpreter's own database can still
    vouch for them as far as its Unicode version allows.

    Private use has not changed since Unicode 2.0, and a noncharacter is never assigned. Unicode
    never unassigns a code point, so a database older than the tables leaves unassigned at least
    everything they call unassigned, a newer one at most that, and one of the same version exactly
    that. Category Cf is the same in Unicode 15.0 and 16.0.
    """
    interpreter = _version(unicodedata.unidata_version)
    tables = _version(UNICODE_VERSION)
    format_codes = {code for low, high in FORMAT_CHARACTER_RANGES for code in range(low, high + 1)}
    for code in range(0x110000):
        character = chr(code)
        category = unicodedata.category(character)
        assert (category == "Co") == is_private_use(character), f"U+{code:04X}"
        if is_noncharacter(character):
            assert category == "Cn", f"U+{code:04X}"
        if interpreter == tables:
            assert (category == "Cn") == is_unassigned(character), f"U+{code:04X}"
        elif interpreter < tables:
            assert category == "Cn" or not is_unassigned(character), f"U+{code:04X}"
        else:
            assert category != "Cn" or is_unassigned(character), f"U+{code:04X}"
        if interpreter in ((15, 0, 0), tables):
            assert (category == "Cf") == (code in format_codes), f"U+{code:04X}"


def _read_json(text: str) -> str | None:
    try:
        value = strict_json_loads(text)
    except ValueError:
        return None
    return json_text(value)


@pytest.mark.parametrize("row", _rows(TEXT, "json"), ids=lambda row: ascii(row)[:60])
def test_strict_json(row: list[object]) -> None:
    """A document read strictly and written back in the shared one-line form, or refused."""
    text, expected = row
    assert _read_json(cast(str, text)) == expected


def test_strict_json_depth_limit_and_no_other_exception() -> None:
    def nested(depth: int) -> str:
        return "[" * depth + "]" * depth

    def objects(depth: int) -> str:
        return '{"a": ' * depth + "1" + "}" * depth

    assert _read_json(nested(JSON_MAX_DEPTH - 1)) is not None
    assert _read_json(nested(JSON_MAX_DEPTH)) is None
    assert _read_json(objects(JSON_MAX_DEPTH - 1)) is not None
    assert _read_json(objects(JSON_MAX_DEPTH)) is None
    # Deep enough for the interpreter's own recursion limit, and integers too large for a float:
    # each must be a plain refusal, never RecursionError or OverflowError.
    assert _read_json(nested(50_000)) is None
    for digits in (309, 310, 1000, 4300, 4301, 10_000):
        _read_json("9" * digits)


@pytest.mark.parametrize("row", _rows(TEXT, "agent"), ids=ascii)
def test_agent(row: list[object]) -> None:
    """Agent inference: one variable order, Unicode White_Space trimmed, names checked."""
    environ, flag, expected = row
    try:
        outcome: dict[str, str] = {
            "agent": _resolve_agent(
                _Globals(agent=cast("str | None", flag)), cast(dict[str, str], environ)
            )
        }
    except ConfigError as exc:
        outcome = {"error": str(exc)}
    assert outcome == expected


@pytest.mark.parametrize("row", _rows(TEXT, "check_text"), ids=ascii)
def test_check_text(row: list[object]) -> None:
    """What ``check`` prints for an admitted command, unsafe characters shown escaped."""
    program, subcommand, rendered, expected = row
    assert (
        check_text(cast(str, program), cast("str | None", subcommand), cast(str, rendered))
        == expected
    )


@pytest.mark.parametrize("row", _rows(TEXT, "run_id"), ids=repr)
def test_run_id(row: list[object]) -> None:
    agent, now, pid, expected = row
    assert make_run_id(cast(str, agent), now=float(cast(int, now)), pid=cast(int, pid)) == expected


# --- configuration loading and admission ----------------------------------------------------------


def _admission(config: Config, home: str | None, command: str) -> dict[str, object]:
    try:
        admission = admit(command, config, home=home)
    except Refused as exc:
        return {"refused": str(exc)}
    except ConfigError as exc:
        return {"config_error": str(exc)}
    return {
        "allowed": {
            "program": admission.program,
            "subcommand": admission.subcommand,
            "rendered": admission.rendered,
        }
    }


def _policy_outcome(config_text: str | None, home: str | None, command: str) -> dict[str, object]:
    try:
        document = None if config_text is None else core_load(config_text)
        config = parse_config(document, source_path=None, project_root="/")
    except YamlError as exc:
        return {"config_error": f"invalid YAML: {exc}"}
    except ConfigError as exc:
        return {"config_error": str(exc)}
    outcome = _admission(config, home, command)
    warning = missing_defaults_warning(config)
    if warning is not None:
        outcome["warning"] = warning
    return outcome


@pytest.mark.parametrize(
    "case", cast(list[dict[str, object]], POLICY["cases"]), ids=lambda case: case["name"]
)
def test_policy(case: dict[str, object]) -> None:
    outcome = _policy_outcome(
        cast("str | None", case["config"]),
        cast("str | None", case["home"]),
        cast(str, case["command"]),
    )
    assert outcome == case["expect"]


def test_the_upgrade_table_is_the_shared_one() -> None:
    """Both editions rewrite exactly the settings the corpus lists, from and to the same text."""
    assert [
        {
            "setting": upgrade.setting,
            "earlier": upgrade.earlier,
            "current": upgrade.current,
            "added": list(upgrade.added),
        }
        for upgrade in UPGRADES
    ] == POLICY["upgrades"]


def _earlier_text(row: dict[str, object]) -> str:
    """The configuration an earlier release wrote, or (``file: null``) the one this release writes."""
    if row["file"] is None:
        return config_template()
    return (_TESTDATA / cast(str, row["file"])).read_text(encoding="utf-8")


def _earlier_config(text: str) -> Config:
    location = cast(dict[str, str], POLICY["earlier_location"])
    return parse_config_text(
        text, source_path=location["source_path"], project_root=location["project_root"]
    )


@pytest.mark.parametrize(
    "row",
    cast(list[dict[str, object]], POLICY["earlier_configs"]),
    ids=lambda row: row["name"],
)
def test_earlier_config(row: dict[str, object]) -> None:
    """A configuration an earlier release wrote: what it admits, the warning, and the upgrade."""
    text = _earlier_text(row)
    config = _earlier_config(text)
    assert missing_defaults_warning(config) == row["warning"]
    assert {program: list(options) for program, options in config.value_options.items()} == row[
        "value_options"
    ]
    for case in cast(list[dict[str, object]], row["commands"]):
        command = cast(str, case["command"])
        assert _admission(config, None, command) == case["expect"], command

    upgraded, applied = upgrade_text(text)
    assert [upgrade.setting for upgrade in applied] == row["upgraded"]
    table = {
        entry["setting"]: entry for entry in cast(list[dict[str, str]], POLICY["upgrades"])
    }
    expected = text
    for setting in cast(list[str], row["upgraded"]):
        assert expected.count(table[setting]["earlier"]) == 1, setting
        expected = expected.replace(table[setting]["earlier"], table[setting]["current"])
    assert upgraded == expected

    after = _earlier_config(upgraded)
    assert missing_defaults_warning(after) == row["warning_after_upgrade"]
    for case in cast(list[dict[str, object]], row["commands_after_upgrade"]):
        command = cast(str, case["command"])
        assert _admission(after, None, command) == case["expect"], command


# --- the configuration YAML subset ----------------------------------------------------------------


def _render(value: object) -> str:
    """The corpus's canonical rendering, written out the same way in ``yaml.rs``."""
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        return quote(value)
    if isinstance(value, list):
        return "[" + ", ".join(_render(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{" + ", ".join(f"{quote(key)}: {_render(item)}" for key, item in value.items()) + "}"
    raise TypeError(f"unexpected YAML value {value!r}")


@pytest.mark.parametrize(
    "case", cast(list[dict[str, str]], YAML["cases"]), ids=lambda case: case["name"]
)
def test_yaml_subset(case: dict[str, str]) -> None:
    if "error" in case:
        with pytest.raises(YamlError) as caught:
            core_load(case["text"])
        assert str(caught.value) == case["error"]
    else:
        assert _render(core_load(case["text"])) == case["value"]


# --- readiness -----------------------------------------------------------------------------------


@pytest.mark.parametrize("row", _rows(READINESS, "prompt"), ids=repr)
def test_prompt_verdict(row: list[object]) -> None:
    text, tail, verdict, reason, last_line = row
    signal = assess_prompt(cast(str, text), cast("str | None", tail))
    assert (signal.verdict, signal.reason, signal.last_line) == (verdict, reason, last_line)


@pytest.mark.parametrize("row", _rows(READINESS, "rc_files"), ids=repr)
def test_prompt_tail_from_rc_file(row: list[object], tmp_path: Path) -> None:
    payload, expected = row
    (tmp_path / ".bashrc").write_bytes(bytes(cast(list[int], payload)))
    assert infer_prompt_tail(Config(), home=str(tmp_path)) == expected


# --- the Herdr protocol --------------------------------------------------------------------------


class _CorpusClient(HerdrClient):
    """The fixed account home and systemd-run path the Rust test client is built with."""

    @staticmethod
    def _account_home() -> str:
        return "/home/account"

    def _systemd_executable(self) -> str:
        return "/usr/bin/systemd-run"


def _protocol_value(client: HerdrClient, method: str, args: list[object]) -> object:
    if method == "server_running":
        return client.server_running()
    if method == "ensure_server":
        return client.ensure_server(attempts=cast(int, args[0]), delay=0.0)
    text = [cast(str, item) for item in args if item is not None]
    if method == "workspace_id_for_label":
        return client.workspace_id_for_label(text[0])
    if method == "tab_id_for_label":
        return client.tab_id_for_label(text[0], text[1])
    if method == "panes":
        panes = client.panes(text[0] if text else None)
        return [[pane.pane_id, pane.tab_id, pane.workspace_id] for pane in panes]
    if method == "process_info":
        info = client.process_info(text[0])
        foreground = [list(entry) for entry in info.foreground]
        return [info.pane_id, info.shell_pid, info.foreground_pgid, foreground]
    if method == "create_workspace":
        return list(client.create_workspace(label=text[0], cwd=text[1]))
    if method == "create_tab":
        return client.create_tab(workspace_id=text[0], label=text[1], cwd=text[2])
    if method == "read":
        return client.read(text[0])
    if method == "run":
        client.run(text[0], text[1])
        return None
    if method == "pane_exists":
        return client.pane_exists(text[0])
    if method == "rename_tab":
        client.rename_tab(text[0], text[1])
        return None
    if method == "close_tab":
        client.close_tab(text[0])
        return None
    raise AssertionError(f"unknown protocol method {method}")


@pytest.mark.parametrize(
    "case",
    cast(list[dict[str, object]], PROTOCOL["cases"]),
    ids=lambda case: f"{case['method']}-{case['args']}",
)
def test_protocol(case: dict[str, object]) -> None:
    calls: list[list[str]] = []
    responses = list(cast(list[list[object]], case["responses"]))

    def runner(command: Sequence[str]) -> subprocess.CompletedProcess[str]:
        calls.append(list(command))
        assert responses, f"unexpected call {command}"
        status, stdout, stderr = responses.pop(0)
        return subprocess.CompletedProcess(
            list(command), cast(int, status), cast(str, stdout), cast(str, stderr)
        )

    client = _CorpusClient(
        herdr_bin=cast(str, PROTOCOL["herdr"]),
        broker=cast(str, case["broker"]),
        run=runner,
        sleep=lambda _seconds: None,
    )
    method = cast(str, case["method"])
    args = cast(list[object], case["args"])
    if "error" in case:
        with pytest.raises(HerdrRunError) as caught:
            _protocol_value(client, method, args)
        assert str(caught.value) == case["error"]
    else:
        assert _protocol_value(client, method, args) == case["ok"]
    assert calls == case["calls"]
    assert not responses


# --- run records and the session cache -----------------------------------------------------------


def _scope_config() -> Config:
    settings = cast(dict[str, str], RECORDS["scope_config"])
    return Config(workspace=settings["workspace"], tab_name=settings["tab_name"], project_root="/p")


@pytest.mark.parametrize("row", _rows(RECORDS, "scope"), ids=repr)
def test_reap_scope(row: list[object]) -> None:
    pane_id, records, expected = row
    got = _scope_of(cast(str, pane_id), cast(list[dict[str, object]], records), _scope_config())
    assert list(got) == expected


@pytest.mark.parametrize("row", _rows(RECORDS, "evidence"), ids=repr)
def test_recorded_evidence(row: list[object]) -> None:
    pane_id, records, flags, identity = row
    got_flags, got_identity = evidence_from_runs(
        cast(str, pane_id), cast(list[dict[str, object]], records)
    )
    assert list(got_flags) == flags
    shown = (
        None
        if got_identity is None
        else [got_identity.pid, got_identity.boot_id, got_identity.start_ticks]
    )
    assert shown == identity


@pytest.mark.parametrize("row", _rows(RECORDS, "cache"), ids=repr)
def test_session_cache_entry(row: list[object], tmp_path: Path) -> None:
    document, expected = row
    path = tmp_path / "session-cache.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    loaded = _load_cache(str(path), "agent-cmds\u0000a")
    shown = (
        None
        if loaded is None
        else [
            loaded.workspace_id,
            loaded.tab_id,
            loaded.pane_id,
            loaded.workspace_label,
            loaded.tab_label,
        ]
    )
    assert shown == expected


@pytest.mark.parametrize("row", _rows(RECORDS, "stored"), ids=repr)
def test_session_cache_store_bytes(row: list[object], tmp_path: Path) -> None:
    existing, key, fields, written, names = row
    path = tmp_path / "spool" / "session-cache.json"
    if existing is not None:
        path.parent.mkdir()
        path.write_text(cast(str, existing), encoding="utf-8")
    workspace_id, tab_id, pane_id, workspace_label, tab_label = cast(list[str], fields)
    target = Target(
        workspace_id=workspace_id,
        tab_id=tab_id,
        pane_id=pane_id,
        workspace_label=workspace_label,
        tab_label=tab_label,
        created=(),
        from_cache=False,
    )
    _store_cache(str(path), cast(str, key), target)
    assert path.read_bytes().decode("utf-8") == written
    assert sorted(entry.name for entry in path.parent.iterdir()) == names


# --- what every child is handed ------------------------------------------------------------------


def _item(value: object) -> bytes:
    """An item of ``child_cases.json``: a string read as UTF-8, or a list of byte values."""
    return value.encode("utf-8") if isinstance(value, str) else bytes(cast(list[int], value))


@pytest.mark.parametrize("row", _rows(CHILD, "environment"), ids=ascii)
def test_child_environment(row: list[object]) -> None:
    entries, changes, expected = row
    built = child_environment(
        [_item(entry) for entry in cast(list[object], entries)],
        [
            (_item(name), None if value is None else _item(value))
            for name, value in cast(list[list[object]], changes)
        ],
    )
    joined = [name + b"=" + value for name, value in built.items()]
    assert joined == [_item(item) for item in cast(list[object], expected)]
    # Applying the rule to its own result changes nothing.
    assert child_environment(joined) == built


#: ``rt_sigaction`` on the two architectures whose kernel ``struct sigaction`` is four eight-byte
#: words (handler, flags, restorer, mask), which is all the helper below declares.
_RT_SIGACTION = {"x86_64": 13, "aarch64": 134}


def _handing_over(signals: Sequence[int]) -> Callable[[], None]:
    """A ``preexec_fn`` that starts the child with exactly ``signals`` ignored, as a caller can.

    Every other signal is first given its default action, so nothing this test process was
    itself handed leaks into the case. Through the system call: the C library refuses to touch 32
    and 33, which it keeps for itself, but a caller is free to leave them ignored, and ``exec``
    keeps them so.
    """
    import ctypes

    libc = ctypes.CDLL(None, use_errno=True)
    number = _RT_SIGACTION[os.uname().machine]
    default = (ctypes.c_uint64 * 4)(0, 0, 0, 0)  # SIG_DFL, no flags, no restorer, empty mask
    ignore = (ctypes.c_uint64 * 4)(1, 0, 0, 0)  # SIG_IGN

    def hand_over() -> None:
        for signal_number in range(1, 65):
            if signal_number not in (9, 19):
                libc.syscall(number, signal_number, default, None, 8)
        for signal_number in signals:
            if libc.syscall(number, signal_number, ignore, None, 8) != 0:
                os._exit(97)

    return hand_over


@pytest.mark.skipif(
    os.uname().machine not in _RT_SIGACTION, reason="the raw sigaction layout is not declared here"
)
@pytest.mark.parametrize("row", _rows(CHILD, "signals"), ids=repr)
def test_child_signals(row: list[object]) -> None:
    """Started with ``row[0]`` ignored, this edition's children have exactly ``row[1]`` ignored.

    The interpreter starts no thread, so nothing takes a signal over; ``subprocess`` gives SIGPIPE
    and SIGXFSZ back their default actions and start-up resets SIGCHLD. The other edition records
    what it was started with and ignores it again in each child.
    """
    started_with, expected = (cast(list[int], value) for value in row)
    probe = (
        "import sys\n"
        "from herdr_run.cli import _prepare_process\n"
        "_prepare_process([])\n"
        "from herdr_run.client import bounded_control_command\n"
        "sys.stdout.write(bounded_control_command(['/bin/cat', '/proc/self/status']).stdout)\n"
    )
    environment = {key: value for key, value in os.environ.items() if not key.startswith("PYTHON")}
    environment.update(PYTHONPATH=str(_REPOSITORY / "py"), LC_ALL="C.UTF-8")
    completed = subprocess.run(
        [sys.executable, "-c", probe],
        env=environment,
        capture_output=True,
        text=True,
        check=False,
        timeout=60,
        preexec_fn=_handing_over(started_with),
    )
    assert completed.returncode == 0, completed.stderr
    line = next(text for text in completed.stdout.splitlines() if text.startswith("SigIgn:"))
    mask = int(line.split(":", 1)[1], 16)
    assert sorted(n for n in range(1, 65) if mask & (1 << (n - 1))) == sorted(expected), line


# --- one version for both editions ---------------------------------------------------------------


def _manifest_version(path: Path, section: str) -> str:
    """The ``version`` line of one TOML section, read without a TOML dependency."""
    current = ""
    for line in path.read_text(encoding="utf-8").splitlines():
        header = re.fullmatch(r"\[([^\]]+)\]", line.strip())
        if header:
            current = header.group(1)
            continue
        found = re.fullmatch(r'version\s*=\s*"([^"]+)"', line.strip())
        if found and current == section:
            return found.group(1)
    raise AssertionError(f"{path} has no version in [{section}]")


def test_python_and_rust_packages_share_one_version() -> None:
    python_project = _manifest_version(_REPOSITORY / "py" / "herdr_run" / "pyproject.toml", "project")
    rust_package = _manifest_version(_REPOSITORY / "rs" / "herdr-run" / "Cargo.toml", "package")
    assert __version__ == python_project == rust_package


if __name__ == "__main__":
    if sys.argv[1:] != ["--write-name-verdicts"]:
        sys.exit(f"usage: {sys.argv[0]} --write-name-verdicts")
    write_name_verdicts()

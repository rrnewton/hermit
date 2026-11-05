"""Tests for tick-hub YAML interchange: YAML is ISOMORPHIC to the JSON schema.

The YAML loader funnels through the same strict narrowing as JSON (a YAML doc and the equivalent
JSON doc build byte-identical canonical JSON), and plain scalars resolve with YAML-1.2 core-schema
rules (so the Norway tokens stay strings) in every implementation.
"""

from __future__ import annotations

from collections.abc import Callable

from tick_hub.io import (
    TickConfigError,
    config_from_json,
    config_from_yaml,
    config_to_json,
    config_to_yaml,
)


def _expect_error(load: Callable[[str], object], text: str) -> None:
    try:
        load(text)
    except TickConfigError:
        return
    raise AssertionError(f"expected TickConfigError for {text!r}")


_YAML = """
# a literate config
description: >-
  the whole
  tick
reminders:
  - name: git_sync
    cadence_secs: 21600
    emit:
      kind: action
      skill: git-sync
      title: sync now
  - name: gated
    gate:
      cmd: echo count=3
      when: nonempty
      capture: true
      timeout_secs: 195
      parallel: true
    emit:
      kind: note
      title: "no"            # quoted Norway token -> string, not bool false
health_checks:
  - name: db
    glob: /var/*.sql
    threshold_secs: 3600
"""

_JSON = (
    '{"description": "the whole tick", "reminders": ['
    '{"name": "git_sync", "cadence_secs": 21600,'
    ' "emit": {"kind": "action", "skill": "git-sync", "title": "sync now"}},'
    '{"name": "gated", "gate": {"cmd": "echo count=3", "when": "nonempty", "capture": true,'
    ' "timeout_secs": 195, "parallel": true},'
    ' "emit": {"kind": "note", "title": "no"}}],'
    ' "health_checks": [{"name": "db", "glob": "/var/*.sql", "threshold_secs": 3600}]}'
)


def test_yaml_isomorphic_to_json() -> None:
    from_yaml = config_from_yaml(_YAML)
    from_json = config_from_json(_JSON)
    assert config_to_json(from_yaml) == config_to_json(from_json)
    assert from_yaml.reminders[1].emit.title == "no"  # stayed a string
    assert from_yaml.reminders[1].gate is not None
    assert from_yaml.reminders[1].gate.timeout_secs == 195
    assert from_yaml.reminders[1].gate.parallel is True
    assert from_yaml.description == "the whole tick"


def test_yaml_emit_round_trips() -> None:
    cfg = config_from_json(_JSON)
    back = config_from_yaml(config_to_yaml(cfg))
    assert config_to_json(back) == config_to_json(cfg)


def test_yaml_plain_norway_tokens_stay_strings() -> None:
    # A plain (unquoted) 'no' in a string field must stay the string "no", not resolve to bool.
    cfg = config_from_yaml(
        "reminders:\n"
        "  - name: r\n"
        "    emit: {kind: note, title: no}\n"
    )
    assert cfg.reminders[0].emit.title == "no"


def test_yaml_bad_when_rejected() -> None:
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: r, gate: {cmd: c, when: yes}, emit: {skill: s}}\n",
    )


def test_yaml_signed_prefixed_integers_are_numeric_and_string_emission_quotes() -> None:
    # YAML 1.2 treats signs on base-prefixed integers consistently with serde_norway.
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: +0o7, emit: {skill: s}}\n",
    )
    cfg = config_from_json(
        '{"reminders": [{"name": "+0o7", "emit": {"skill": "s"}}]}'
    )
    emitted = config_to_yaml(cfg)
    assert "'+0o7'" in emitted
    assert config_to_json(config_from_yaml(emitted)) == config_to_json(cfg)


def test_yaml_rejects_non_string_and_duplicate_mapping_keys() -> None:
    _expect_error(config_from_yaml, "1: value\n")
    _expect_error(config_from_yaml, "description: one\ndescription: two\n")
    _expect_error(
        config_from_yaml,
        "defaults: &defaults {cadence_secs: 1}\n<<: *defaults\nreminders: []\n",
    )
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: r, emit: {skill: s, fields: {\"<<\": value}}}\n",
    )


def test_yaml_rejects_nonfinite_numbers_even_in_nullable_fields() -> None:
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: r, gate: .nan, emit: {skill: s}}\n",
    )
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: r, cadence_secs: .inf, emit: {skill: s}}\n",
    )


def test_dependency_edges_round_trip_and_reject_invalid_graphs() -> None:
    text = (
        "reminders:\n"
        "  - {name: foundation, gate: {cmd: f}, emit: {skill: s}}\n"
        "  - {name: dependent, depends_on: [foundation], gate: {cmd: d}, emit: {skill: s}}\n"
    )
    config = config_from_yaml(text)
    assert config.reminders[1].depends_on == ("foundation",)
    assert config_from_yaml(config_to_yaml(config)) == config

    invalid = (
        "reminders:\n"
        "  - {name: a, depends_on: [%s], gate: {cmd: a}, emit: {skill: s}}\n"
        "  - {name: b, depends_on: [%s], gate: {cmd: b}, emit: {skill: s}}\n"
    )
    _expect_error(config_from_yaml, invalid % ("missing", ""))
    _expect_error(config_from_yaml, invalid % ("a", ""))
    _expect_error(config_from_yaml, invalid % ("b", "a"))
    _expect_error(
        config_from_yaml,
        "reminders:\n  - {name: a, depends_on: [a, a], gate: {cmd: a}, emit: {skill: s}}\n",
    )

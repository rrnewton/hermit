"""Tests for YAML interchange: YAML is ISOMORPHIC to the JSON schema.

The YAML loader funnels through the same strict narrowing as JSON, so a YAML document and the
equivalent JSON document must build byte-identical canonical JSON. These tests also pin the
adversarial cases (block scalars and the quoted Norway-problem tokens) so a parser change that
diverged from the schema would fail loudly.
"""

from __future__ import annotations

import json
from collections.abc import Callable

from dagrun.io import (
    DagJsonError,
    dag_from_json,
    dag_from_yaml,
    dag_to_json,
    dag_to_yaml,
)


def _expect_dag_error(load: Callable[[str], object], text: str) -> None:
    """Assert ``load(text)`` raises :class:`DagJsonError` (kept pytest-free like the rest of this
    file so mypy's ``py/`` typecheck never follows pytest into third-party stubs)."""
    try:
        load(text)
    except DagJsonError:
        return
    raise AssertionError(f"expected DagJsonError for {text!r}")

# A YAML document exercising: a literal (|-) block-scalar description with quotes + unicode, a
# folded (>-) block scalar, a QUOTED Norway-problem token ("no") that must stay the STRING "no"
# (not the bool false), a quoted number-like string, and YAML comments.
_YAML = """
# top-level description as a folded block scalar (newlines fold to spaces)
description: >-
  the whole
  pipeline
resource_caps:
  browser: 1
steps:
  - group: build
    job: app
    desc: compile               # short label
    description: |-             # literal block scalar: newlines preserved, no trailing newline
      line 1
      line 2 with "quotes" and unicode é☃
    cmd: make build
    hint:
      classification: cpu-bound
      preferred_inner_jobs: 8
  - group: e2e
    job: smoke
    desc: browser
    description: "no"            # quoted Norway token -> string, not bool false
    cmd: make e2e
    deps: [build.app]
    hint:
      resources:
        browser: 1
"""

# The JSON document that MUST load to the same DagConfig. Note ">- the whole\\n  pipeline" folds
# to "the whole pipeline" (single space, no trailing newline).
_JSON = (
    '{"description": "the whole pipeline", "resource_caps": {"browser": 1}, "steps": ['
    '{"group": "build", "job": "app", "desc": "compile",'
    ' "description": "line 1\\nline 2 with \\"quotes\\" and unicode é☃",'
    ' "cmd": "make build",'
    ' "hint": {"classification": "cpu-bound", "preferred_inner_jobs": 8}},'
    '{"group": "e2e", "job": "smoke", "desc": "browser", "description": "no",'
    ' "cmd": "make e2e", "deps": ["build.app"], "hint": {"resources": {"browser": 1}}}]}'
)


def test_yaml_isomorphic_to_json() -> None:
    from_yaml = dag_from_yaml(_YAML)
    from_json = dag_from_json(_JSON)
    # Isomorphism: identical canonical JSON regardless of input syntax.
    assert dag_to_json(from_yaml) == dag_to_json(from_json)
    # The quoted Norway token stayed a string; the literal block scalar chomped correctly.
    assert from_yaml.steps[1].description == "no"
    assert from_yaml.steps[0].description == 'line 1\nline 2 with "quotes" and unicode é☃'
    assert from_yaml.description == "the whole pipeline"


def test_yaml_emit_round_trips() -> None:
    cfg = dag_from_json(
        '{"description": "d", "steps": [{"group": "g", "job": "j", "desc": "x",'
        ' "description": "multi\\nline", "cmd": "true"}]}'
    )
    # dag_to_yaml output need not match Rust, but must round-trip back to the same DagConfig.
    back = dag_from_yaml(dag_to_yaml(cfg))
    assert dag_to_json(back) == dag_to_json(cfg)


def test_yaml_emit_round_trips_exotic_strings() -> None:
    """String descriptions that the core-schema loader would otherwise re-read as a number/bool/
    null (``1e3``, ``0o17``, the empty string, ...) must be quoted on emit so dag_to_yaml round
    trips through dag_from_yaml unchanged."""
    exotic = [
        "1e3", "0o17", "0x10", "no", "yes", "on", "off", "2024-01-01", "1_000",
        "true", "null", "0755", "1:20", ".inf", ".nan", "", "0b101", "1.5", "  spaces  ",
    ]
    for desc in exotic:
        doc = json.dumps(
            {"description": desc, "steps": [{"group": "g", "job": "j", "cmd": "c"}]}
        )
        cfg = dag_from_json(doc)
        back = dag_from_yaml(dag_to_yaml(cfg))
        assert dag_to_json(back) == dag_to_json(cfg), f"round-trip changed json for {desc!r}"
        assert back.description == desc, f"round-trip changed value for {desc!r}"


def test_yaml_plain_scalars_resolve_like_yaml_1_2_core() -> None:
    """Plain (unquoted) scalars resolve with serde_norway's YAML-1.2 core rules, NOT PyYAML's 1.1
    defaults: the Norway tokens stay strings, and leading-zero/underscore/sexagesimal/timestamp
    tokens are strings too."""
    cfg = dag_from_yaml(
        "steps:\n"
        "  - group: g\n"
        "    job: j\n"
        "    cmd: make\n"
        "    description: no\n"  # plain 'no' -> string, not False (Norway problem)
    )
    assert cfg.steps[0].description == "no"
    # 0o/0x/0b integer forms and no-dot exponent floats DO resolve (matching serde_norway).
    cfg2 = dag_from_yaml(
        "default_step_timeout: 0o17\nsteps: [{group: g, job: j, cmd: make}]\n"
    )
    assert cfg2.default_step_timeout == 15


def test_yaml_bool_field_rejects_norway_strings() -> None:
    """A boolean field accepts only true/false; yes/no/on/off are strings and must be rejected —
    the same way the Rust build rejects them."""
    for tok in ("no", "yes", "on", "off"):
        _expect_dag_error(
            dag_from_yaml,
            f"steps:\n  - {{group: g, job: j, cmd: make, networkonly: {tok}}}\n",
        )


def test_json_rejects_non_finite() -> None:
    """Non-finite JSON floats (accepted by Python's json by default) are rejected, matching the
    Rust build's serde_json."""
    docs = [
        '{"mem_cap_factor": Infinity, "steps": []}',
        '{"mem_cap_factor": -Infinity, "steps": []}',
        '{"mem_cap_factor": NaN, "steps": []}',
        '{"mem_cap_factor": 1e400, "steps": []}',
    ]
    for doc in docs:
        _expect_dag_error(dag_from_json, doc)


def test_json_and_yaml_reject_out_of_i64_range_int() -> None:
    """Integers beyond signed 64-bit are rejected on both paths (the Rust model uses i64)."""
    big = "99999999999999999999999999"
    _expect_dag_error(dag_from_json, f'{{"mem_cap_floor_bytes": {big}, "steps": []}}')
    _expect_dag_error(
        dag_from_yaml,
        f"mem_cap_floor_bytes: {big}\nsteps: [{{group: g, job: j, cmd: make}}]\n",
    )


def test_yaml_non_finite_scalar_is_null_not_infinity() -> None:
    """A YAML `.inf` in a nullable float field reads as null (matching serde_norway), and in a
    non-nullable float field it is rejected — never an accepted infinity."""
    cfg = dag_from_yaml(
        "steps:\n  - {group: g, job: j, cmd: make, hint: {measured_effective_cores: .inf}}\n"
    )
    assert cfg.steps[0].hint.measured_effective_cores is None
    _expect_dag_error(
        dag_from_yaml, "mem_cap_factor: .inf\nsteps: [{group: g, job: j, cmd: make}]\n"
    )

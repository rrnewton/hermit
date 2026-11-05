"""Tests for tick_hub.state: strict typed ops-state, YAML round-trip, state-machine lines."""

from __future__ import annotations

import pytest

from tick_hub.state import (
    DEFAULT_TICK_FREQUENCY_MIN,
    OpsState,
    StateError,
    flag_truthy,
    state_lines,
)


def _expect_state_error(text: str) -> None:
    try:
        OpsState.from_yaml(text)
    except StateError:
        return
    raise AssertionError(f"expected StateError for {text!r}")


def test_full_valid_state() -> None:
    state = OpsState.from_yaml(
        "enabled: true\n"
        "tick_frequency_min: 45\n"
        "label: my-host\n"
        "flags:\n"
        "  benchmark_enabled: true\n"
        "  region: no\n"      # plain 'no' -> string (Norway-safe)
        "  count: 3\n"
    )
    assert state.enabled is True
    assert state.tick_frequency_min == 45
    assert state.label == "my-host"
    assert state.flags == {"benchmark_enabled": True, "region": "no", "count": 3}


def test_default_state() -> None:
    d = OpsState.default()
    assert d.enabled is True
    assert d.tick_frequency_min == DEFAULT_TICK_FREQUENCY_MIN
    assert d.flags == {}


def test_yaml_round_trip_including_string_flag() -> None:
    s = OpsState(
        enabled=False,
        tick_frequency_min=10,
        label="h",
        flags={"a": True, "region": "no", "n": 7},
    )
    back = OpsState.from_yaml(s.to_yaml())
    assert back == s
    assert isinstance(back.flags["region"], str)  # not coerced to bool


def test_direct_invalid_state_is_rejected_before_serialization() -> None:
    with pytest.raises(StateError):
        OpsState(enabled=True, tick_frequency_min=0).to_yaml()
    with pytest.raises(StateError):
        OpsState(enabled=True, tick_frequency_min=30, flags={"<<": True}).to_yaml()
    canonical = OpsState(enabled=True, tick_frequency_min=30, label=" host ").to_yaml()
    assert OpsState.from_yaml(canonical).label == "host"


def test_strict_validation() -> None:
    _expect_state_error("tick_frequency_min: 30\n")  # missing enabled
    _expect_state_error("enabled: 1\ntick_frequency_min: 30\n")  # enabled not bool
    _expect_state_error("enabled: true\ntick_frequency_min: 0\n")  # non-positive
    _expect_state_error("enabled: true\ntick_frequency_min: nope\n")  # not int
    _expect_state_error("enabled: true\ntick_frequency_min: 9223372036854775808\n")
    _expect_state_error("enabled: true\ntick_frequency_min: 30\nunknown_key: 1\n")
    _expect_state_error("enabled: true\ntick_frequency_min: 30\nlabel: 5\n")  # label not str
    _expect_state_error("enabled: true\ntick_frequency_min: 30\nflags: notamap\n")
    _expect_state_error("enabled: true\ntick_frequency_min: 30\nflags: null\n")
    _expect_state_error(
        "enabled: true\ntick_frequency_min: 30\nflags:\n  bad: [1, 2]\n"  # flag value not scalar
    )
    _expect_state_error(
        "enabled: true\ntick_frequency_min: 30\nflags:\n  too_big: 9223372036854775808\n"
    )
    _expect_state_error(
        "enabled: true\ntick_frequency_min: 30\nflags:\n  too_small: -9223372036854775809\n"
    )
    _expect_state_error(
        "enabled: true\ntick_frequency_min: 30\nflags:\n  \"<<\": value\n"
    )


def test_flag_integer_i64_endpoints() -> None:
    state = OpsState.from_yaml(
        "enabled: true\n"
        "flags:\n"
        "  low: -9223372036854775808\n"
        "  high: 9223372036854775807\n"
    )
    assert state.flags == {
        "low": -9223372036854775808,
        "high": 9223372036854775807,
    }


def test_flag_truthy() -> None:
    flags: dict[str, bool | int | str] = {
        "t": True, "f": False, "one": 1, "zero": 0, "s": "x", "empty": ""
    }
    assert flag_truthy(flags, "t") is True
    assert flag_truthy(flags, "f") is False
    assert flag_truthy(flags, "one") is True
    assert flag_truthy(flags, "zero") is False
    assert flag_truthy(flags, "s") is True
    assert flag_truthy(flags, "empty") is False
    assert flag_truthy(flags, "absent") is False


def test_state_lines_summary_and_actualize() -> None:
    state = OpsState(enabled=True, tick_frequency_min=30, label="h", flags={"x": True})
    lines = state_lines(state, current_tick_min=15)
    assert lines[0].startswith("NOTE: ops-state enabled=true tick_frequency_min=30 label=h")
    assert "flags=x=true" in lines[0]
    assert any(ln.startswith("ACTION: actualize-tick-frequency") for ln in lines)
    # matching cadence -> no actualize action
    lines2 = state_lines(state, current_tick_min=30)
    assert not any(ln.startswith("ACTION: actualize-tick-frequency") for ln in lines2)


def test_state_lines_disabled_note() -> None:
    state = OpsState(enabled=False, tick_frequency_min=30)
    lines = state_lines(state, current_tick_min=None)
    assert any("disabled" in ln for ln in lines)

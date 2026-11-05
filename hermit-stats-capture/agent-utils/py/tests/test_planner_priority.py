"""Adversarial checks for the standalone planner priority providers."""

from __future__ import annotations

import pytest

from pr_landing_planner.priority import (
    DEFAULT_LABEL_PATTERN,
    DEFAULT_PRIORITY,
    CommandPriority,
    LabelPriority,
    make_priority_provider,
)


def test_label_priority_rejects_missing_capture_and_i64_overflow() -> None:
    with pytest.raises(ValueError, match="capture group"):
        LabelPriority(r"^p[0-9]+$")

    source = LabelPriority(r"^p(.+)$")
    assert source.priority(8, ["p9223372036854775808"]) == DEFAULT_PRIORITY
    assert source.last_error is not None
    assert "signed 64-bit ASCII" in source.last_error


def test_command_priority_requires_command_and_rejects_malformed_output() -> None:
    with pytest.raises(ValueError, match="non-empty command"):
        make_priority_provider("command", command="  ")

    source = CommandPriority("printf '%s' '{pr}'")
    assert source.priority(7, ()) == 7
    assert source.last_error is None

    overflow = CommandPriority("printf 9223372036854775808")
    assert overflow.priority(9, ()) == DEFAULT_PRIORITY
    assert overflow.last_error is not None
    assert "signed 64-bit ASCII" in overflow.last_error

    provider = make_priority_provider(
        "command", label_pattern=DEFAULT_LABEL_PATTERN, command="printf '1 extra'"
    )
    assert provider.priority(10, ()) == DEFAULT_PRIORITY
    assert provider.last_error is not None

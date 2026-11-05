"""Tests for the tick-hub line formatters (exact-bytes output contract)."""

from __future__ import annotations

from tick_hub.emit import format_action, format_error, format_health, format_note


def test_action_bare_and_quoted_fields() -> None:
    line = format_action(
        "ci-health-red",
        {"branch": "main", "runs": "run 1 failed", "empty": ""},
        title='CI on main is "red"',
    )
    # bare when safe; quoted (with escaped inner quote) when it has spaces/quotes/is empty.
    assert line == (
        'ACTION: ci-health-red branch=main runs="run 1 failed" empty="" '
        'title="CI on main is \\"red\\""'
    )


def test_action_without_title_omits_title() -> None:
    line = format_action("integrate-green-pr", {"pr": "42", "ci": "green"}, "")
    assert line == "ACTION: integrate-green-pr pr=42 ci=green"


def test_note_and_error() -> None:
    assert format_note("hello world") == "NOTE: hello world"
    assert format_error("boom") == "ERROR: boom"


def test_health_ok_stale_missing() -> None:
    assert format_health("db", "ok", 10, 100, "snap") == (
        'HEALTH: db ok age_secs=10 threshold_secs=100 detail="snap"'
    )
    assert format_health("db", "missing", None, 100, "snap") == (
        'HEALTH: db missing age_secs=NA threshold_secs=100 detail="snap"'
    )
    assert "stale" in format_health("db", "stale", 999, 100, "snap")

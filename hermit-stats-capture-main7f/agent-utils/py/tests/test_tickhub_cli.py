"""Tests for the tick-hub CLI surface (in-process, stdlib capture)."""

from __future__ import annotations

import contextlib
import io
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from tick_hub import __version__
from tick_hub.cli import PROG, STATE_FILE_ENV, _load_userguide, main

_CONFIG = """
reminders:
  - name: sync
    cadence_secs: 21600
    emit: {kind: action, skill: git-sync, title: sync now}
  - name: backlog
    gate: {cmd: "echo count=7", when: always, capture: true}
    emit:
      kind: action
      skill: triage
      fields: {threshold: "5"}
      title: "{count} ready (> {threshold})"
  - name: bench
    requires_flags: [benchmark_enabled]
    emit: {kind: action, skill: run-benchmark, title: refresh}
health_checks:
  - name: db
    glob: /this/does/not/exist-*.sql
    threshold_secs: 3600
    detail: newest snapshot
"""

_STATE = "enabled: true\ntick_frequency_min: 30\nlabel: h\nflags:\n  benchmark_enabled: true\n"


@pytest.fixture(autouse=True)
def _isolated_fired_state(
    tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch
) -> Iterator[Path]:
    """Point the default fired-state at a throwaway dir so no tick writes into the repo checkout."""
    store = tmp_path_factory.mktemp("fired_state") / "state"
    monkeypatch.setenv(STATE_FILE_ENV, str(store))
    yield store


def _capture(args: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        rc = main(args)
    return rc, out.getvalue(), err.getvalue()


def _write(tmp: Path, name: str, text: str) -> str:
    path = tmp / name
    path.write_text(text, encoding="utf-8")
    return str(path)


def test_no_args_prints_help() -> None:
    rc, out, _ = _capture([])
    assert rc == 0
    assert PROG in out and "quickstart" in out


def test_quickstart_is_self_contained() -> None:
    rc, out, _ = _capture(["quickstart"])
    assert rc == 0
    for marker in ("Install", "output contract", "Reminders", "ops-state", "Exit codes"):
        assert marker in out
    assert "python3 -m pip install tick-hub" in out
    assert "agent-utils" not in out and "cargo install" not in out


def test_userguide_prints_embedded_guide() -> None:
    """`--userguide` prints the full embedded guide VERBATIM (byte-for-byte, no coloring)."""
    rc, out, _ = _capture(["--userguide"])
    assert rc == 0
    embedded = _load_userguide()
    assert out == embedded
    assert len(out) > 3000
    assert "tick-hub" in out


def test_list(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    rc, out, _ = _capture(["list", "--config", cfg])
    assert rc == 0
    assert "sync" in out and "backlog" in out and "db" in out


def test_json_and_yaml_render(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    rc, out, _ = _capture(["json", "--config", cfg])
    assert rc == 0 and '"reminders"' in out
    rc, out, _ = _capture(["yaml", "--config", cfg])
    assert rc == 0 and "reminders:" in out


def test_tick_dry_run_emits_expected_lines(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    state = _write(tmp_path, "s.yaml", _STATE)
    rc, out, err = _capture(
        ["tick", "--config", cfg, "--state", state, "--now", "0", "--no-header"]
    )
    assert rc == 0
    assert "HEALTH: db missing age_secs=NA" in out
    assert "NOTE: ops-state enabled=true tick_frequency_min=30 label=h" in out
    assert "ACTION: git-sync" in out
    assert 'ACTION: triage threshold=5 count=7 title="7 ready (> 5)"' in out
    assert "ACTION: run-benchmark" in out
    assert out.strip().splitlines()[-1] == "NOTE: emitted 3 instruction(s) this tick"
    assert "dry-run" in err  # dry-run notice on stderr


def test_tick_dry_run_does_not_persist(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    fired = tmp_path / "fired"
    rc, _, _ = _capture(["tick", "--config", cfg, "--now", "0", "--fired-state", str(fired)])
    assert rc == 0
    assert not fired.exists()  # dry-run writes nothing


def test_tick_flush_persists(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    fired = tmp_path / "fired"
    rc, _, err = _capture(
        ["tick", "--config", cfg, "--now", "1000", "--fired-state", str(fired), "--flush"]
    )
    assert rc == 0
    assert fired.exists()
    body = fired.read_text(encoding="utf-8")
    assert "sync=1000" in body and "backlog=1000" in body
    assert "persisted" in err


def test_tick_report_pending_is_compatible_with_flush_and_cadence(tmp_path: Path) -> None:
    cfg = _write(
        tmp_path,
        "quiet.yaml",
        """
reminders:
  - name: quiet
    cadence_secs: 3600
    gate: {cmd: "true", when: failure}
    emit: {kind: action, skill: warn, title: problem}
""",
    )
    fired = tmp_path / "fired"
    rc, out, err = _capture(
        [
            "tick",
            "--config",
            cfg,
            "--now",
            "1000",
            "--fired-state",
            str(fired),
            "--flush",
            "--report-pending",
            "--no-header",
        ]
    )
    assert rc == 0
    assert "CLEAN: quiet ran and found nothing to report" in out
    assert "quiet=1000" in fired.read_text(encoding="utf-8")
    assert "persisted" in err

    rc, out, _ = _capture(
        [
            "tick",
            "--config",
            cfg,
            "--now",
            "1001",
            "--fired-state",
            str(fired),
            "--report-pending",
            "--no-header",
        ]
    )
    assert rc == 0
    assert "CLEAN: quiet" not in out, "reporting must not bypass cadence"


def test_tick_flush_failure_is_a_controlled_usage_error(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    fired = tmp_path / "fired-dir"
    fired.mkdir()
    rc, _, err = _capture(
        ["tick", "--config", cfg, "--now", "1000", "--fired-state", str(fired), "--flush"]
    )
    assert rc == 2
    assert "cannot persist fired-state" in err
    assert not (tmp_path / "fired-dir.tmp").exists()


def test_tick_actualize_tick_frequency(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    state = _write(tmp_path, "s.yaml", _STATE)
    rc, out, _ = _capture(
        ["tick", "--config", cfg, "--state", state, "--now", "0",
         "--current-tick-min", "15", "--no-header"]
    )
    assert rc == 0
    assert "ACTION: actualize-tick-frequency desired=30 current=15" in out


def test_tick_flag_off_suppresses_reminder(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    state = _write(tmp_path, "s.yaml", "enabled: true\ntick_frequency_min: 30\nflags:\n  benchmark_enabled: false\n")
    rc, out, _ = _capture(["tick", "--config", cfg, "--state", state, "--now", "0", "--no-header"])
    assert rc == 0
    assert "run-benchmark" not in out


def test_tick_missing_state_uses_default(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    rc, out, err = _capture(["tick", "--config", cfg, "--state", str(tmp_path / "nope.yaml"), "--now", "0"])
    assert rc == 0
    assert "not found" in err  # loud fallback notice
    assert "NOTE: ops-state enabled=true" in out


def test_tick_bad_config_exits_2(tmp_path: Path) -> None:
    bad = _write(tmp_path, "bad.json", "not json")
    rc, _, err = _capture(["tick", "--config", bad, "--now", "0"])
    assert rc == 2
    assert "invalid JSON" in err


def test_tick_malformed_state_exits_2(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    bad_state = _write(tmp_path, "bad.yaml", "enabled: notabool\ntick_frequency_min: 30\n")
    rc, _, err = _capture(["tick", "--config", cfg, "--state", bad_state, "--now", "0"])
    assert rc == 2
    assert "invalid ops-state" in err


def test_state_subcommand(tmp_path: Path) -> None:
    state = _write(tmp_path, "s.yaml", _STATE)
    rc, out, _ = _capture(["state", "--state", state, "--current-tick-min", "60"])
    assert rc == 0
    assert "NOTE: ops-state enabled=true" in out
    assert "ACTION: actualize-tick-frequency desired=30 current=60" in out


def test_missing_config_file_exits_2() -> None:
    rc, _, err = _capture(["list", "--config", "/nonexistent/nope.yaml"])
    assert rc == 2
    assert PROG in err


@pytest.mark.parametrize("flag", ["--now", "--current-tick-min"])
def test_tick_numeric_flags_are_signed_i64(flag: str, tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    with pytest.raises(SystemExit) as raised:
        main(["tick", "--config", cfg, flag, "9223372036854775808"])
    assert raised.value.code == 2


def test_state_current_tick_numeric_flag_is_signed_i64(tmp_path: Path) -> None:
    state = _write(tmp_path, "s.yaml", _STATE)
    with pytest.raises(SystemExit) as raised:
        main(["state", "--state", state, "--current-tick-min", "-9223372036854775809"])
    assert raised.value.code == 2


@pytest.mark.parametrize(
    ("command", "flag", "value"),
    [
        ("tick", "--now", "-1"),
        ("tick", "--current-tick-min", "0"),
        ("state", "--current-tick-min", "0"),
    ],
)
def test_cli_numeric_flags_respect_domain(
    command: str, flag: str, value: str, tmp_path: Path
) -> None:
    first_flag = "--config" if command == "tick" else "--state"
    document = _CONFIG if command == "tick" else _STATE
    path = _write(tmp_path, f"{command}.yaml", document)
    with pytest.raises(SystemExit) as raised:
        main([command, first_flag, path, flag, value])
    assert raised.value.code == 2


def test_long_option_abbreviations_are_rejected(tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    with pytest.raises(SystemExit) as raised:
        main(["tick", "--conf", cfg, "--now", "0"])
    assert raised.value.code == 2


@pytest.mark.parametrize("value", ["1_0", " 10", "١٠"])
def test_cli_integer_syntax_is_ascii_decimal(value: str, tmp_path: Path) -> None:
    cfg = _write(tmp_path, "c.yaml", _CONFIG)
    with pytest.raises(SystemExit) as raised:
        main(["tick", "--config", cfg, "--now", value])
    assert raised.value.code == 2


def test_version_via_module() -> None:
    # Run in the py/ package root so `-m tick_hub` resolves regardless of pytest's invocation cwd.
    py_root = Path(__file__).resolve().parent.parent
    result = subprocess.run(
        [sys.executable, "-m", "tick_hub", "--version"],
        capture_output=True,
        text=True,
        check=True,
        cwd=str(py_root),
    )
    assert result.stdout.strip() == f"{PROG} {__version__}"

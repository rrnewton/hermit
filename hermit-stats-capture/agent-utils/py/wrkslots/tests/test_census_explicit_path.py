"""Explicit accounting-path errors use the real CLI's controlled refusal path."""

from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path

import pytest

from wrkslots import cli
from wrkslots.tests.test_census_cost import _config


def _read_only_audit_fixture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[cli.Config, Path, Path]:
    config = _config(tmp_path)
    checkout = config.worktrees / "unregistered"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    artifact = cache / "artifact"
    artifact.write_bytes(b"unchanged cache content")
    identity = cli._open_directory_identity(checkout, "fixture checkout")
    directory = cli.CacheDirectory(cache, checkout, *identity)

    # Registry observations are synthetic and read-only; main, audit dispatch,
    # path validation, cache traversal and report generation remain real.
    monkeypatch.setattr(cli, "_load_config", lambda *_args: config)
    monkeypatch.setattr(cli, "_locked", lambda *_args, **_kwargs: contextlib.nullcontext())
    monkeypatch.setattr(cli, "_refuse_partial_state", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(cli, "_validate_global_state", lambda *_args: ((), ()))
    monkeypatch.setattr(cli, "_audit_validate_batch_seal_evidence", lambda *_args: ((), ()))
    monkeypatch.setattr(cli, "_all_journal_cache_slots", lambda *_args, **_kwargs: ())
    monkeypatch.setattr(cli, "_cache_slot_directories", lambda *_args, **_kwargs: (directory,))
    return config, cache, artifact


def _run_audit(config: cli.Config, state_path: Path) -> int:
    return cli.main(
        [
            "--project-root", str(config.root), "audit", "--format", "json",
            "--cache-census-state", str(state_path),
        ]
    )


def test_explicit_symlink_cycle_is_cli_refusal_without_rows_or_state_files(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, _cache, artifact = _read_only_audit_fixture(tmp_path, monkeypatch)
    first = tmp_path / "loop-a"
    second = tmp_path / "loop-b"
    first.symlink_to(second)
    second.symlink_to(first)
    before = sorted(str(path.relative_to(tmp_path)) for path in tmp_path.rglob("*"))

    result = _run_audit(config, first / "census.json")

    output = capsys.readouterr()
    assert result == 3
    assert output.out == ""
    assert output.err.startswith("REFUSED: audit cache census state parent must already exist:")
    assert "REMEDY:" in output.err
    assert "Traceback" not in output.err
    assert sorted(str(path.relative_to(tmp_path)) for path in tmp_path.rglob("*")) == before
    assert artifact.read_bytes() == b"unchanged cache content"
    assert os.readlink(first) == str(second)
    assert os.readlink(second) == str(first)


def test_explicit_canonical_parent_publishes_real_cache_measurement(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, cache, artifact = _read_only_audit_fixture(tmp_path, monkeypatch)
    storage = tmp_path / "storage"
    storage.mkdir()
    state_path = storage / "census.json"

    result = _run_audit(config, state_path)

    output = capsys.readouterr()
    assert result == 0
    assert output.err == ""
    report = json.loads(output.out)
    assert len(report["slots"]) == 1
    row = report["slots"][0]
    assert row["slot"] == "unregistered"
    assert row["verdict"] == "BLOCKED"
    assert row["cache_status"] == "complete"
    assert row["cache_error"] is None
    assert row["cache_bytes"] == sum(
        path.stat().st_blocks * 512 for path in (cache, artifact)
    )
    assert sorted(path.name for path in storage.iterdir()) == [
        "census.json", "census.json.key"
    ]
    assert artifact.read_bytes() == b"unchanged cache content"

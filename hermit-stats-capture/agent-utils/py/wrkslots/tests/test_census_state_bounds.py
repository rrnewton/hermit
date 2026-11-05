"""Regenerable census state must fail locally, within the same read/write cap."""

from __future__ import annotations

import hashlib
import hmac
import json
import os
from pathlib import Path

import pytest

from wrkslots import cli
from wrkslots.tests.test_census_cost import _config


def _payload() -> dict[str, object]:
    return {
        "schema": cli._AUDIT_CACHE_CENSUS_SCHEMA,
        "registry_revision": "b" * 64,
        "roots": {},
        "next_subject": "snowman-\u2603",
    }


def _canonical(payload: dict[str, object]) -> bytes:
    return json.dumps(
        payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("ascii")


@pytest.mark.parametrize(
    "contents",
    [
        b'{"x":' + b"9" * 5000 + b"}",
        b'{"x":' + b"[" * 10000 + b"0" + b"]" * 10000 + b"}",
        b'{"broken":',
    ],
    ids=["integer-conversion-limit", "decoder-recursion-limit", "ordinary-json-error"],
)
def test_bounded_invalid_state_rebuilds_and_returns_exact_measurement(
    tmp_path: Path, contents: bytes
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    output = cache / "output"
    output.write_bytes(b"current content")
    identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(cache, checkout, *identity)
    state_path = tmp_path / "census.json"
    state_path.write_bytes(contents)
    assert len(contents) < cli._AUDIT_CACHE_STATE_BYTES_LIMIT

    measured, counters = cli._audit_cache_census(
        config,
        (cli.ActiveState("testhost", 1, ()),),
        {"subject": (directory,)},
        {},
        state_path=state_path,
        work_limit=100,
        wall_seconds=5,
    )

    assert measured["subject"].status == "complete"
    assert measured["subject"].bytes == sum(
        path.stat().st_blocks * 512 for path in (cache, output)
    )
    assert counters["directories_visited"] > 0
    assert counters["work_consumed"] <= 100
    rebuilt = json.loads(state_path.read_bytes())
    mac = rebuilt.pop("hmac_sha256")
    key = state_path.with_name(state_path.name + ".key").read_bytes()
    assert mac == hmac.new(key, _canonical(rebuilt), hashlib.sha256).hexdigest()
    assert rebuilt["schema"] == cli._AUDIT_CACHE_CENSUS_SCHEMA


def test_authentication_rejects_decodable_excessive_nesting(tmp_path: Path) -> None:
    """Depth below the decoder limit must also be bounded during MAC encoding."""

    nested: object = 0
    for _ in range(64):
        nested = [nested]
    payload = {**_payload(), "roots": {"hostile": nested}}
    mac = hmac.new(b"k" * 32, _canonical(payload), hashlib.sha256).hexdigest()
    envelope = {**payload, "hmac_sha256": mac}
    state_path = tmp_path / "census.json"
    state_path.write_bytes(_canonical(envelope))
    parent_fd = os.open(tmp_path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        assert cli._read_audit_cache_state(
            state_path, parent_fd, b"k" * 32, "b" * 64
        ) is None
    finally:
        os.close(parent_fd)


def test_writer_uses_identical_exact_read_cap_and_authentication(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload = _payload()
    key = b"k" * 32
    mac = hmac.new(key, _canonical(payload), hashlib.sha256).hexdigest()
    expected = _canonical({**payload, "hmac_sha256": mac})
    monkeypatch.setattr(cli, "_AUDIT_CACHE_STATE_BYTES_LIMIT", len(expected))
    state_path = tmp_path / "census.json"
    parent_fd = os.open(tmp_path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        cli._write_audit_cache_state(state_path, payload, key, parent_fd)
        assert state_path.read_bytes() == expected
        assert cli._read_audit_cache_state(
            state_path, parent_fd, key, "b" * 64
        ) == payload
    finally:
        os.close(parent_fd)


def test_writer_refuses_one_byte_over_cap_before_opening_any_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    payload = _payload()
    key = b"k" * 32
    mac = hmac.new(key, _canonical(payload), hashlib.sha256).hexdigest()
    expected = _canonical({**payload, "hmac_sha256": mac})
    monkeypatch.setattr(cli, "_AUDIT_CACHE_STATE_BYTES_LIMIT", len(expected) - 1)
    state_path = tmp_path / "census.json"
    state_path.write_bytes(b"previous progress")
    parent_fd = os.open(tmp_path, os.O_RDONLY | os.O_DIRECTORY)

    def unexpected_open(*args: object, **kwargs: object) -> int:
        raise AssertionError("oversize state created an output before refusal")

    monkeypatch.setattr(os, "open", unexpected_open)
    try:
        with pytest.raises(cli.Refusal, match="byte safety bound"):
            cli._write_audit_cache_state(state_path, payload, key, parent_fd)
        assert state_path.read_bytes() == b"previous progress"
        assert list(tmp_path.iterdir()) == [state_path]
    finally:
        os.close(parent_fd)


@pytest.mark.parametrize("kind", ["deep", "integer", "cyclic"])
def test_unencodable_state_refuses_before_creating_output(
    tmp_path: Path, kind: str
) -> None:
    nested: object
    if kind == "deep":
        nested = 0
        for _ in range(10000):
            nested = [nested]
    elif kind == "integer":
        nested = 10**5000
    else:
        cycle: list[object] = []
        cycle.append(cycle)
        nested = cycle
    payload = {**_payload(), "roots": {"hostile": nested}}
    state_path = tmp_path / "census.json"
    state_path.write_bytes(b"previous progress")
    parent_fd = os.open(tmp_path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        with pytest.raises(cli.Refusal, match="nesting bound|safely encoded"):
            cli._write_audit_cache_state(state_path, payload, b"k" * 32, parent_fd)
        assert state_path.read_bytes() == b"previous progress"
        assert list(tmp_path.iterdir()) == [state_path]
    finally:
        os.close(parent_fd)


def test_unpersistable_progress_is_explicit_error_not_silent_restart(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    config = _config(tmp_path)
    checkout = config.root / "checkout"
    cache = checkout / "target"
    cache.mkdir(parents=True)
    (cache / "output").write_bytes(b"content")
    identity = cli._open_directory_identity(checkout, "checkout")
    directory = cli.CacheDirectory(cache, checkout, *identity)
    state_path = tmp_path / "census.json"
    monkeypatch.setattr(cli, "_AUDIT_CACHE_STATE_BYTES_LIMIT", 128)

    for _ in range(3):
        measured, counters = cli._audit_cache_census(
            config,
            (cli.ActiveState("testhost", 1, ()),),
            {"subject": (directory,), "empty": (), "refused": ()},
            {"refused": "original binding refusal"},
            state_path=state_path,
            work_limit=2,
            wall_seconds=5,
        )
        assert measured["subject"].status == "error"
        assert measured["subject"].bytes is None
        assert "128-byte safety bound" in (measured["subject"].error or "")
        assert measured["empty"].status == "complete"
        assert measured["empty"].bytes == 0
        assert measured["refused"].error == "original binding refusal"
        assert counters["work_consumed"] <= 2
        assert not state_path.exists()
    assert sorted(path.name for path in tmp_path.iterdir()) == [
        "census.json.key", "project"
    ]

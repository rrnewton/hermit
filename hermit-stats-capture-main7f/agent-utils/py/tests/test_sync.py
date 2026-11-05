"""Tests for the pluggable profile-summary sync backends (dagrun.sync).

The local + git-branch backends are exercised end-to-end offline (the git backend against a local
bare repo, including the retry-on-conflict read-modify-write). The github-artifacts backend's pure
selection logic and the spec parser are unit-tested; its network path needs a live runner."""

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from dagrun import summary as S
from dagrun import sync

MID = "m"
CC = "affinity8_cpu-max-max"


def _row(step: str, inner: int, elapsed: float, peak: int) -> dict[str, str]:
    return {
        "step": step,
        "inner_jobs": str(inner),
        "elapsed_s": f"{elapsed:.3f}",
        "peak_bytes": str(peak),
        "pct_other": "0.000",
    }


def _delta(step: str, elapsed: float) -> S.Summary:
    return S.summary_from_rows([_row(step, 1, elapsed, 1000)], MID, CC, 8)


# --------------------------------------------------------------------------- spec parsing


def test_parse_backend_dispatch() -> None:
    assert isinstance(sync.parse_backend("local:/tmp/x"), sync.LocalDirBackend)
    assert isinstance(sync.parse_backend("git:/tmp/repo#data"), sync.GitBranchBackend)
    assert isinstance(sync.parse_backend("git:/tmp/repo#data#sub/dir"), sync.GitBranchBackend)
    assert isinstance(sync.parse_backend("github-artifacts:sum#o/r"), sync.GitHubArtifactsBackend)


def test_parse_backend_rejects_bad_specs() -> None:
    for spec in (
        "",
        "nonsense",
        "local:",
        "git:/tmp/repo",
        "git:#branch",
        "s3:",
        "s3:bucket/prefix",
        "bogus:x",
    ):
        with pytest.raises(sync.SyncError):
            sync.parse_backend(spec)


def test_select_latest_artifact() -> None:
    arts = [
        {"name": "sum", "id": 1, "created_at": "2026-01-01T00:00:00Z", "expired": False},
        {"name": "sum", "id": 2, "created_at": "2026-02-01T00:00:00Z", "expired": False},
        {"name": "sum", "id": 3, "created_at": "2026-03-01T00:00:00Z", "expired": True},
        {"name": "other", "id": 4, "created_at": "2026-04-01T00:00:00Z", "expired": False},
    ]
    latest = sync._select_latest_artifact(arts, "sum")
    assert latest is not None and latest["id"] == 2  # newest non-expired matching name
    assert sync._select_latest_artifact(arts, "missing") is None


# --------------------------------------------------------------------------- local backend


def test_local_backend_roundtrip_and_accumulate(tmp_path: Path) -> None:
    backend = sync.LocalDirBackend(tmp_path / "store")
    assert not S.summary_stats(backend.download(MID, CC))[0]  # empty at first
    backend.publish(_delta("g.a", 1.0))
    backend.publish(_delta("g.b", 2.0))
    got = backend.download(MID, CC)
    buckets, total, _largest = S.summary_stats(got)
    assert buckets == 2 and total == 2  # both contributions retained


# --------------------------------------------------------------------------- git-branch backend


def _init_bare_repo(path: Path) -> str:
    subprocess.run(["git", "init", "--bare", "-q", str(path)], check=True)
    return str(path)


def test_git_backend_roundtrip(tmp_path: Path) -> None:
    url = _init_bare_repo(tmp_path / "bare.git")
    backend = sync.GitBranchBackend(url, "profile-data")
    assert not S.summary_stats(backend.download(MID, CC))[0]  # branch absent -> empty
    backend.publish(_delta("g.a", 1.0))
    got = backend.download(MID, CC)
    assert S.summary_stats(got) == (1, 1, 1)
    backend.publish(_delta("g.b", 2.0))
    assert S.summary_stats(backend.download(MID, CC)) == (2, 2, 1)


def test_git_backend_subdir(tmp_path: Path) -> None:
    url = _init_bare_repo(tmp_path / "bare.git")
    backend = sync.GitBranchBackend(url, "profile-data", "nested/profiles")
    backend.publish(_delta("g.a", 1.0))
    assert S.summary_stats(backend.download(MID, CC)) == (1, 1, 1)


def test_git_backend_retry_on_conflict_does_not_clobber(tmp_path: Path) -> None:
    """Simulate a concurrent contributor: a hook pushes a DIFFERENT delta to the branch right before
    our first push, forcing a non-fast-forward rejection. The retry must re-fetch the new tip and
    re-merge OUR delta, so BOTH contributions survive (no clobber = correct RMW)."""
    url = _init_bare_repo(tmp_path / "bare.git")
    concurrent = sync.GitBranchBackend(url, "profile-data")  # a separate "runner"
    injected = {"done": False}

    def inject_conflict(attempt: int) -> None:
        if not injected["done"]:
            injected["done"] = True
            concurrent.publish(_delta("g.concurrent", 9.0))  # lands on the branch first

    ours = sync.GitBranchBackend(url, "profile-data", before_push=inject_conflict)
    merged = ours.publish(_delta("g.ours", 1.0))
    # The returned merged summary and the branch must contain BOTH steps.
    steps = {step for (step, _inner, _workload) in merged.buckets}
    assert steps == {"g.concurrent", "g.ours"}
    final = sync.GitBranchBackend(url, "profile-data").download(MID, CC)
    assert {step for (step, _inner, _workload) in final.buckets} == {
        "g.concurrent",
        "g.ours",
    }
    assert injected["done"]  # the conflict path actually fired

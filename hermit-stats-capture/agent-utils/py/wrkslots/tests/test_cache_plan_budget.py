"""Audit cache-glob planning is bounded, fair and faithful to ``Path.glob``."""

from __future__ import annotations

import contextlib
import dataclasses
import fnmatch
import functools
import itertools
import json
import os
import random
import re
import subprocess
import sys
import types
from collections.abc import Callable, Iterator, Sequence
from pathlib import Path

import pytest

from wrkslots import cli
from wrkslots.tests.test_census_cost import _config

LIVE_SHAPED = (
    "node_modules",
    "target",
    "ignored/**/target",
    "ignored/**/target-debug",
    "ignored/**/components-target",
)


def _tree(tmp_path: Path) -> Path:
    root = tmp_path / "checkout"
    outside = tmp_path / "outside"
    (outside / "target").mkdir(parents=True)
    for directory in (
        "target",
        "ignored/a/target/nested/target",
        "ignored/a/b/c/target-debug",
        "ignored/a/bb/target",
        "ignored/.hidden/components-target",
        "ignored/tfile",
        "ignored/slink",
        "ignored/broken",
        "ignored/deep/b/x/y/target-debug",
        "a/bb/d",
        "a/bc/d",
        "a/bcd/d",
        "a/x/d",
    ):
        (root / directory).mkdir(parents=True)
    (root / "ignored/a/target/deep-file").write_bytes(b"x")
    (root / "ignored/a/target/nested/target-debug").write_bytes(b"x")
    (root / "ignored/tfile/target").write_bytes(b"x")
    (root / "ignored/slink/target").symlink_to(outside / "target", target_is_directory=True)
    (root / "ignored/broken/target").symlink_to(root / "absent")
    (root / "ignored/link").symlink_to(outside, target_is_directory=True)
    (root / "node_modules").symlink_to(root / "target", target_is_directory=True)
    return root


def _expand(root: Path, pattern: str) -> list[Path]:
    ((expanded_pattern, found),) = cli._expand_cache_globs(root, (pattern,))
    assert expanded_pattern == pattern
    return list(found)


@pytest.mark.parametrize(
    "pattern",
    (
        *LIVE_SHAPED,
        "ignored/*/target",
        "ignored/link/target",
        "ignored/**/*/target",
        "ignored/*/**/target",
        "ignored/**/b/**/target-debug",
        "ignored/**/**/target",
        "a/b?/d",
        "a/[bc]*/d",
        "a/*/d",
        "missing/**/target",
    ),
)
def test_expansion_yields_what_path_glob_yields(tmp_path: Path, pattern: str) -> None:
    root = _tree(tmp_path)
    expected = list(root.glob(pattern))
    found = _expand(root, pattern)
    assert sorted(found) == sorted(expected)
    assert len(found) == len(set(found))
    parts = pattern.split("/")
    if "**" not in parts or (
        parts.count("**") == 1
        and parts[-2] == "**"
        and not any(magic in "".join(parts[:-2]) for magic in "*?[")
    ):
        # Only these shapes are also yielded in Path.glob's order, so the first
        # refusal a checkout reports is unchanged. Other shapes, such as
        # 'ignored/*/**/target', may name a different first unsafe path.
        assert found == expected


def test_trailing_recursive_wildcard_yields_directories_without_following_links(
    tmp_path: Path,
) -> None:
    # Python 3.13 also yields files here; the 3.12 behavior is the contract.
    root = _tree(tmp_path)
    expected = [
        Path(directory)
        for directory, _names, _files in os.walk(root / "ignored", followlinks=False)
    ]
    assert sorted(_expand(root, "ignored/**")) == sorted(expected)


def test_unreadable_directory_contributes_nothing(tmp_path: Path) -> None:
    root = _tree(tmp_path)
    closed = root / "ignored/a"
    closed.chmod(0)
    try:
        if os.access(closed, os.R_OK):
            pytest.skip("directory permissions are not enforced for this user")
        for pattern in LIVE_SHAPED:
            expected = list(root.glob(pattern))
            assert _expand(root, pattern) == expected
        assert not any(
            closed in path.parents for path in _expand(root, "ignored/**/target")
        )
    finally:
        closed.chmod(0o755)


@contextlib.contextmanager
def _counted_listings(monkeypatch: pytest.MonkeyPatch) -> Iterator[list[str]]:
    listed: list[str] = []
    real = os.scandir

    def scandir(path: str | os.PathLike[str]) -> Iterator[os.DirEntry[str]]:
        listed.append(os.fspath(path))
        return real(path)

    with monkeypatch.context() as patch:
        patch.setattr(os, "scandir", scandir)
        yield listed


def test_joint_expansion_lists_each_directory_once(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = _tree(tmp_path)
    budget = cli._CachePlanBudget(
        cli._AuditWorkBudget(1_000), 3_600.0, 3_600.0
    )
    with _counted_listings(monkeypatch) as listed:
        expanded = cli._expand_cache_globs(root, LIVE_SHAPED, budget)
    # The checkout root, then every directory below ignored/ reached without
    # following a symlink, including the insides of matched directories.
    below = [
        directory
        for directory, _names, _files in os.walk(root / "ignored", followlinks=False)
    ]
    assert sorted(listed) == sorted([str(root), *below])
    assert len(listed) == len(set(listed))
    assert budget.work.consumed == len(listed)
    assert expanded == tuple(
        (pattern, tuple(root.glob(pattern))) for pattern in LIVE_SHAPED
    )


def _chain(root: Path, depth: int) -> Path:
    leaf = root / "ignored"
    for index in range(depth):
        leaf = leaf / f"d{index:03d}"
    (leaf / "target").mkdir(parents=True)
    return leaf / "target"


def test_exhausted_work_share_stops_listing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "checkout"
    _chain(root, 30)
    budget = cli._CachePlanBudget(
        cli._AuditWorkBudget(5), 3_600.0, 3_600.0
    )
    with _counted_listings(monkeypatch) as listed:
        with pytest.raises(cli._CachePlanExhausted) as refused:
            cli._expand_cache_globs(root, ("ignored/**/target",), budget)
    assert str(refused.value) == (
        "cache planning exhausted this subject's share of 5 directory listings; "
        "no cache total can be published within this allowance"
    )
    assert len(listed) == 5
    assert budget.work.consumed == 5


def test_walk_that_outlasts_its_wall_share_stops_listing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "checkout"
    _chain(root, 30)
    ticks = iter(range(1_000))
    monkeypatch.setattr(
        cli, "time", types.SimpleNamespace(monotonic=lambda: float(next(ticks)))
    )
    budget = cli._CachePlanBudget(cli._AuditWorkBudget(1_000), 4.0, 8.0, spent=1.0)
    with _counted_listings(monkeypatch) as listed:
        with pytest.raises(cli._CachePlanExhausted) as refused:
            cli._expand_cache_globs(root, ("ignored/**/target",), budget)
    assert str(refused.value) == (
        "cache planning exhausted this subject's share of the fixed wall allowance "
        "of 8 seconds; no cache total can be published within this allowance"
    )
    # The walk starts at tick 0 and reads one tick per listing: the checks at
    # ticks 1 and 2 leave 1 + 1 and 1 + 2 of 4 seconds spent, tick 3 refuses.
    assert len(listed) == 2
    assert budget.spent == 1.0 + 4.0


def test_expired_wall_share_refuses_before_any_listing_or_git(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = tmp_path / "checkout"
    _chain(root, 3)
    budget = cli._CachePlanBudget(cli._AuditWorkBudget(1_000), 2.5, 2.5, spent=2.5)

    class NoGit(cli._GitVcs):
        def repository_root(self, repository: Path) -> Path:
            raise AssertionError("Git ran after the planning allowance expired")

    expired = (
        "cache planning exhausted this subject's share of the fixed wall allowance "
        "of 2.5 seconds; no cache total can be published within this allowance"
    )
    with _counted_listings(monkeypatch) as listed:
        with pytest.raises(cli._CachePlanExhausted) as refused:
            cli._expand_cache_globs(root, ("ignored/**/target",), budget)
        assert str(refused.value) == expired
        with pytest.raises(cli._CachePlanExhausted) as refused:
            cli._cache_directories_for_path(
                _config(tmp_path), root, "checkout", NoGit(),
                ("ignored/**/target",), budget=budget,
            )
        assert str(refused.value) == expired
    assert listed == []
    assert budget.work.consumed == 0


def _git_checkout(path: Path) -> None:
    path.mkdir(parents=True)
    subprocess.run(["git", "init", "-q", str(path)], check=True)
    subprocess.run(
        [
            "git", "-C", str(path), "-c", "user.name=fixture",
            "-c", "user.email=fixture@example.invalid",
            "commit", "-q", "--allow-empty", "-m", "fixture",
        ],
        check=True,
    )


def _planning_audit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[cli.Config, Path, Path]:
    config = dataclasses.replace(_config(tmp_path), cache_globs=("ignored/**/target",))
    deep = config.worktrees / "a-deep" / "repo"
    cheap = config.worktrees / "b-cheap" / "repo"
    _git_checkout(deep)
    _git_checkout(cheap)
    deep_cache = _chain(deep, 200)
    (deep_cache / "artifact").write_bytes(b"deep cache content")
    cheap_cache = cheap / "ignored" / "x" / "target"
    cheap_cache.mkdir(parents=True)
    (cheap_cache / "artifact").write_bytes(b"cheap cache content")
    # Registry observations are synthetic and read-only; planning, Git
    # observations, cache traversal and report generation remain real.
    monkeypatch.setattr(cli, "_load_config", lambda *_args: config)
    monkeypatch.setattr(cli, "_locked", lambda *_args, **_kwargs: contextlib.nullcontext())
    monkeypatch.setattr(cli, "_refuse_partial_state", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(cli, "_validate_global_state", lambda *_args: ((), ()))
    monkeypatch.setattr(cli, "_audit_validate_batch_seal_evidence", lambda *_args: ((), ()))
    monkeypatch.setattr(cli, "_all_journal_cache_slots", lambda *_args, **_kwargs: ())
    return config, deep_cache, cheap_cache


def _run_audit(config: cli.Config, tmp_path: Path, *extra: str) -> int:
    storage = tmp_path / "storage"
    storage.mkdir(exist_ok=True)
    return cli.main(
        [
            "--project-root", str(config.root), "audit", "--format", "json",
            "--cache-census-state", str(storage / "census.json"), *extra,
        ]
    )


def _allocated(cache: Path) -> int:
    return sum(path.stat().st_blocks * 512 for path in (cache, cache / "artifact"))


def test_expensive_subject_exhausts_only_its_own_planning_share(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, _deep_cache, cheap_cache = _planning_audit(tmp_path, monkeypatch)

    result = _run_audit(config, tmp_path, "--cache-work-limit", "60")

    output = capsys.readouterr()
    assert result == 0, output.err
    report = json.loads(output.out)
    rows = {row["slot"]: row for row in report["slots"]}
    assert sorted(rows) == ["a-deep", "b-cheap"]
    # The costly subject is planned first and still leaves its peer an equal
    # share: 60 // 2 listings each, of which the cheap peer needs four.
    assert rows["a-deep"]["cache_status"] == "error"
    assert rows["a-deep"]["cache_bytes"] is None
    exhausted = (
        "cache planning exhausted this subject's share of 30 directory listings; "
        "no cache total can be published within this allowance"
    )
    assert rows["a-deep"]["cache_error"] == exhausted
    # These directories have no registry row, which is always BLOCKED, so the
    # verdict here says nothing about exhaustion. The registered-record test
    # below pins that exhaustion itself becomes a blocking reason.
    assert rows["a-deep"]["verdict"] == "BLOCKED"
    assert rows["b-cheap"]["cache_status"] == "complete"
    assert rows["b-cheap"]["cache_error"] is None
    assert rows["b-cheap"]["cache_bytes"] == _allocated(cheap_cache)
    phases = {phase["name"]: phase for phase in report["metrics"]["phases"]}
    assert phases["cache-planning"]["work"] == {
        "directories_listed": 34,
        "exhausted_subjects": 1,
        "subjects": 2,
    }


def test_sufficient_planning_share_measures_every_subject(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, deep_cache, cheap_cache = _planning_audit(tmp_path, monkeypatch)

    result = _run_audit(config, tmp_path)

    output = capsys.readouterr()
    assert result == 0, output.err
    rows = {row["slot"]: row for row in json.loads(output.out)["slots"]}
    assert rows["a-deep"]["cache_status"] == "complete"
    assert rows["a-deep"]["cache_bytes"] == _allocated(deep_cache)
    assert rows["b-cheap"]["cache_status"] == "complete"
    assert rows["b-cheap"]["cache_bytes"] == _allocated(cheap_cache)


def test_planning_still_refuses_a_nested_symlinked_cache_name(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, deep_cache, _cheap_cache = _planning_audit(tmp_path, monkeypatch)
    # Inside an already-matched directory, as Path.glob also reaches it.
    nested = deep_cache / "inner" / "target"
    nested.parent.mkdir()
    nested.symlink_to(tmp_path, target_is_directory=True)

    result = _run_audit(config, tmp_path)

    output = capsys.readouterr()
    assert result == 0, output.err
    rows = {row["slot"]: row for row in json.loads(output.out)["slots"]}
    assert rows["a-deep"]["cache_status"] == "error"
    assert rows["a-deep"]["cache_error"] == f"cache path crosses a symlink: {nested}"
    assert rows["b-cheap"]["cache_status"] == "complete"


def test_planning_shares_divide_what_earlier_subjects_left(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    config, _deep_cache, _cheap_cache = _planning_audit(tmp_path, monkeypatch)
    shares: list[tuple[str, float, float, int]] = []
    real = cli._cache_slot_directories

    def spy(
        config: cli.Config,
        cache_slot: cli.CacheSlot,
        *,
        vcs: cli._GitVcs | None = None,
        budget: cli._CachePlanBudget | None = None,
    ) -> tuple[cli.CacheDirectory, ...]:
        assert budget is not None
        shares.append((cache_slot.slot, budget.seconds, budget.spent, budget.work.limit))
        planned = real(config, cache_slot, vcs=vcs, budget=budget)
        shares.append((cache_slot.slot, budget.seconds, budget.spent, budget.work.consumed))
        return planned

    monkeypatch.setattr(cli, "_cache_slot_directories", spy)
    result = _run_audit(
        config, tmp_path, "--cache-work-limit", "1000", "--cache-wall-seconds", "1000"
    )

    assert result == 0, capsys.readouterr().err
    (deep, deep_seconds, deep_before, deep_limit) = shares[0]
    (_, _, deep_spent, deep_used) = shares[1]
    (cheap, cheap_seconds, cheap_before, cheap_limit) = shares[2]
    assert (deep, cheap) == ("a-deep", "b-cheap")
    # Half of each allowance for the first of two subjects; everything that is
    # left for the last. Only walking is charged, and it was charged.
    assert (deep_seconds, deep_limit) == (500.0, 500)
    assert (cheap_seconds, cheap_limit) == (1000.0 - deep_spent, 1000 - deep_used)
    assert deep_before == cheap_before == 0.0
    assert 0.0 < deep_spent < 500.0
    assert deep_used == 203


def test_registered_record_planning_uses_its_share(tmp_path: Path) -> None:
    from wrkslots.tests.test_lifecycle import checkout, command, create, make_project

    project, _repository, _remote = make_project(
        tmp_path, cache_globs=("ignored/**/target",)
    )
    assert create(project).returncode == 0
    cache = _chain(checkout(project), 40)
    (cache / "artifact").write_bytes(b"registered cache content")

    bounded = command(project, "audit", "--format", "json", "--cache-work-limit", "10")
    complete = command(project, "audit", "--format", "json")

    assert bounded.returncode == 0, bounded.stderr
    row = {row["slot"]: row for row in json.loads(bounded.stdout)["slots"]}["slot01"]
    assert row["cache_status"] == "error"
    exhausted = (
        "cache planning exhausted this subject's share of 10 directory listings; "
        "no cache total can be published within this allowance"
    )
    assert row["cache_error"] == exhausted
    assert row["verdict"] == "BLOCKED"
    assert f"cache inspection failed: {exhausted}" in row["reasons"]
    assert complete.returncode == 0, complete.stderr
    row = {row["slot"]: row for row in json.loads(complete.stdout)["slots"]}["slot01"]
    assert row["cache_status"] == "complete"
    assert row["cache_bytes"] == _allocated(cache)
    assert not [r for r in row["reasons"] if r.startswith("cache inspection")]


def _tracked_by_every_glob(
    vcs: cli._GitVcs, checkout: Path, cache_globs: Sequence[str]
) -> tuple[str, ...]:
    """``_GitVcs.tracked_cache_paths`` as it was before the index: the same
    body, with ``vcs`` for ``self``, so the matcher also runs at the same
    stack depth when both are called from the same frame."""

    if not cache_globs:
        return ()
    indexed = vcs._run(checkout, ["ls-files", "--recurse-submodules", "-z"])
    committed = vcs._run(
        checkout,
        ["ls-tree", "-r", "--name-only", "-z", "HEAD"],
    )
    paths = [
        path
        for output in (indexed.stdout, committed.stdout)
        for path in output.split("\x00")
        if path
        and any(
            cli._cache_glob_contains_path(pattern, path) for pattern in cache_globs
        )
    ]
    return tuple(dict.fromkeys(paths))


LISTED_CHECKOUT = Path("/listed-checkout-is-never-read")


def _listed(indexed: Sequence[str], committed: Sequence[str] = ()) -> cli._GitVcs:
    """A Git boundary whose index and HEAD listings are given, not read."""

    listings = {
        "ls-files": "".join(f"{path}\x00" for path in indexed),
        "ls-tree": "".join(f"{path}\x00" for path in committed),
    }

    class Listed(cli._GitVcs):
        @staticmethod
        def _run(
            repository: Path, args: Sequence[str], **options: object
        ) -> subprocess.CompletedProcess[str]:
            assert repository == LISTED_CHECKOUT and not options
            return subprocess.CompletedProcess(list(args), 0, listings[args[0]], "")

    return Listed()


# '**', wildcards and classes in the first component (which validation refuses
# but direct callers may pass), '?' in and after the first component, dotfiles,
# a trailing slash, leading and doubled separators, an unclosed '[', literal
# ']' and '\', and globs with no components at all.
TRICKY_GLOBS = (
    *LIVE_SHAPED,
    "**",
    "**/target",
    "*/target",
    "t*",
    "t?rget",
    "?arget/**",
    "ignored/?/target",
    "a/?/**",
    "[tT]arget",
    "[!x]arget",
    "t[a-c]rget",
    ".cache",
    ".cache/**",
    ".*",
    "target/",
    "/target",
    "ignored//target",
    "./target",
    "a/b/c",
    "a/[bc]/**",
    "a\\b",
    "x]y",
    "[",
    "a/**/**/c",
    "",
    "/",
)
COMPONENTS = (
    "target", "Target", "xarget", "targets", ".cache", ".target", "ignored",
    "a", "b", "c", "node_modules", "[", "a\\b", "x]y", ".", "",
)
# Every join of up to three components, so empty components produce leading,
# trailing and doubled separators, and the empty path itself is included.
TRICKY_PATHS = tuple(
    "/".join(combination)
    for length in range(0, 4)
    for combination in itertools.product(COMPONENTS, repeat=length)
)


def test_cache_path_selection_equals_testing_every_glob() -> None:
    assert len(TRICKY_PATHS) == 1 + 16 + 16**2 + 16**3
    # HEAD lists a subset in reverse plus paths the index lacks, so the union
    # and its order are exercised too.
    vcs = _listed(
        TRICKY_PATHS,
        (*reversed(TRICKY_PATHS[::7]), "target/head-only", ".cache/head-only"),
    )
    generator = random.Random(20261003)
    glob_sets: list[tuple[str, ...]] = [
        LIVE_SHAPED,
        TRICKY_GLOBS,
        *((pattern,) for pattern in TRICKY_GLOBS),
        *(
            tuple(generator.sample(TRICKY_GLOBS, generator.randint(2, 6)))
            for _ in range(20)
        ),
    ]
    selected_somewhere = 0
    for cache_globs in glob_sets:
        expected = _tracked_by_every_glob(vcs, LISTED_CHECKOUT, cache_globs)
        assert vcs.tracked_cache_paths(LISTED_CHECKOUT, cache_globs) == expected, (
            cache_globs
        )
        selected_somewhere += bool(expected)
    # The comparison is not vacuous: most glob sets select some paths.
    assert selected_somewhere > len(glob_sets) // 2


def _spy_on_matcher(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, str]]:
    tested: list[tuple[str, str]] = []
    real = cli._cache_glob_contains_path

    def spy(pattern: str, path: str) -> bool:
        tested.append((pattern, path))
        return real(pattern, path)

    monkeypatch.setattr(cli, "_cache_glob_contains_path", spy)
    return tested


def test_cache_path_selection_tests_only_globs_sharing_the_first_component(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    tested = _spy_on_matcher(monkeypatch)
    compared: list[str] = []
    real_fnmatchcase = fnmatch.fnmatchcase

    def fnmatchcase_spy(name: str, pattern: str) -> bool:
        compared.append(pattern)
        return real_fnmatchcase(name, pattern)

    monkeypatch.setattr(fnmatch, "fnmatchcase", fnmatchcase_spy)
    source = [f"src/module{index}/target/file{index}.rs" for index in range(10_000)]
    paths = [*source, "target/debug/x.o", "ignored/a/target/y", "ignored/a/b", "targets/z"]
    vcs = _listed(paths)

    assert vcs.tracked_cache_paths(LISTED_CHECKOUT, LIVE_SHAPED) == (
        "target/debug/x.o",
        "ignored/a/target/y",
    )
    # Each candidate is tested against the globs that begin with its first
    # component, in order, until one accepts it. Testing a glob of the form
    # 'ignored/**/...' against a path outside 'ignored' still passes 'ignored'
    # to fnmatch, so the first source path is tested against those three, and
    # so is 'targets/z', which follows tests that used fnmatch. The other
    # 9,999 source paths would make the same three tests right after them,
    # which would change nothing, so they cause no test at all.
    expected_run = (
        "ignored/**/target",
        "ignored/**/target-debug",
        "ignored/**/components-target",
    )
    assert tested == [
        *((pattern, source[0]) for pattern in expected_run),
        ("target", "target/debug/x.o"),
        ("ignored/**/target", "ignored/a/target/y"),
        ("ignored/**/target", "ignored/a/b"),
        ("ignored/**/target-debug", "ignored/a/b"),
        ("ignored/**/components-target", "ignored/a/b"),
        *((pattern, "targets/z") for pattern in expected_run),
    ]
    assert len(compared) < 100

    # A glob with no literal first component is still tested against every path.
    tested.clear()
    assert vcs.tracked_cache_paths(LISTED_CHECKOUT, ("**/file7.rs",)) == (
        "src/module7/target/file7.rs",
    )
    assert len(tested) == len(paths)


def _first_component(text: str) -> str:
    return next((part for part in text.split("/") if part), "")


def _spy_on_matcher_and_fnmatch(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, ...]]:
    """Record, in order, each matcher call as ('test', pattern, path) and each
    pattern passed to ``fnmatch`` as ('fnmatch', pattern)."""

    events: list[tuple[str, ...]] = []
    real_fnmatchcase = fnmatch.fnmatchcase
    real_matcher = cli._cache_glob_contains_path

    def fnmatchcase_spy(name: str, pattern: str) -> bool:
        events.append(("fnmatch", pattern))
        return real_fnmatchcase(name, pattern)

    def matcher_spy(pattern: str, path: str) -> bool:
        events.append(("test", pattern, path))
        return real_matcher(pattern, path)

    monkeypatch.setattr(fnmatch, "fnmatchcase", fnmatchcase_spy)
    monkeypatch.setattr(cli, "_cache_glob_contains_path", matcher_spy)
    return events


def _tests_with_recency(
    events: Sequence[tuple[str, ...]],
) -> tuple[list[tuple[str, str, tuple[str, ...], tuple[str, ...]]], tuple[str, ...]]:
    """Each matcher call with the patterns passed to ``fnmatch`` before it,
    least recently passed first, and the patterns it passed for the first time;
    and the recency order at the end.

    fnmatch keeps compiled patterns in a least-recently-used cache, so two
    runs that start from the same cache and pass patterns in the same recency
    order at every point leave it holding the same patterns, whatever its size.
    A pattern passed for the first time is one fnmatch, and ``re`` beneath it,
    had to compile.
    """

    recency: dict[str, None] = {}
    calls: list[tuple[str, str, tuple[str, ...], list[str]]] = []
    for event in events:
        if event[0] == "fnmatch":
            if event[1] not in recency:
                assert calls, "fnmatch was used outside a matcher call"
                calls[-1][3].append(event[1])
            recency.pop(event[1], None)
            recency[event[1]] = None
        else:
            calls.append((event[1], event[2], tuple(recency), []))
    tests = [(pattern, path, before, tuple(new)) for pattern, path, before, new in calls]
    return tests, tuple(recency)


def test_cache_path_selection_tests_globs_in_their_original_order(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    events = _spy_on_matcher_and_fnmatch(monkeypatch)
    # Anchored and unanchored globs interleaved, so an index that tested the
    # anchored ones first would call the matcher in a different order. Two
    # anchored '**' globs with different first components come after the last
    # unanchored one: testing them against a path with no components reaches
    # fnmatch for neither, so testing either for such a path would reorder
    # what the path before it left. The last glob passes 'ignored' to fnmatch
    # again after 'b', so 'ignored' must end up the more recent of the two.
    interleaved = (
        "ignored/**/target", "t*", "target/", "**/c", "ignored/a", "[ab]/**",
        "target", "a/b", "*/target", "ignored/**/target-debug", "a/*/x/y",
        "b/**/q", "ignored/**/x",
    )
    # With only anchored globs, nothing between a path with no components and
    # the next path uses fnmatch, so the next path must not take the tests made
    # for the first, which reached fnmatch for none, as already made.
    anchored = ("ignored/**/target", "b/**/q", "a/*/x")
    for cache_globs, paths in (
        (interleaved, TRICKY_PATHS),
        (interleaved, ("target/x", "ignored/a/b/c", "a/b/c", "zz/c", "/")),
        (anchored, ("/", "zz/c", "//", "zz/c/d", "a", "zz/c/d")),
    ):
        vcs = _listed(paths)
        events.clear()
        expected = _tracked_by_every_glob(vcs, LISTED_CHECKOUT, cache_globs)
        every_glob_calls, every_glob_final = _tests_with_recency(events)
        events.clear()
        assert vcs.tracked_cache_paths(LISTED_CHECKOUT, cache_globs) == expected
        made, final = _tests_with_recency(events)
        # The index makes the calls testing every glob makes, in the same
        # order, leaving some out, and only calls to a glob whose literal first
        # component differs from the path's, which compile nothing. Before each
        # call it makes, and at the end, fnmatch has seen the same patterns in
        # the same recency order, so its cache holds what it held before at
        # every point, and it compiled the same patterns in the same order.
        position = 0
        left_out = 0
        for pattern, path, recency, compiled in every_glob_calls:
            if position < len(made) and made[position][:2] == (pattern, path):
                if made[position][2:] != (recency, compiled):
                    pytest.fail(
                        f"before testing {(pattern, path)} fnmatch saw, and the "
                        f"test compiled, {made[position][2:]}; testing every "
                        f"glob {(recency, compiled)}"
                    )
                position += 1
                continue
            left_out += 1
            first = _first_component(pattern)
            if (
                compiled
                or not first
                or any(character in first for character in "*?[")
                or first == _first_component(path)
            ):
                pytest.fail(f"left out {(pattern, path)}, which compiled {compiled}")
        if position != len(made):
            pytest.fail(f"call {position} of the index, {made[position][:2]}, is out of order")
        assert final == every_glob_final
        assert left_out > 0


def _every_glob_outcome(
    vcs: cli._GitVcs, cache_globs: Sequence[str]
) -> tuple[str, ...] | None:
    try:
        return _tracked_by_every_glob(vcs, LISTED_CHECKOUT, cache_globs)
    except RecursionError:
        return None


def _indexed_outcome(
    vcs: cli._GitVcs, cache_globs: Sequence[str]
) -> tuple[str, ...] | None:
    try:
        return vcs.tracked_cache_paths(LISTED_CHECKOUT, cache_globs)
    except RecursionError:
        return None


def _deep_glob(count: int) -> str:
    return "target/" + "/".join(["**"] * count)


def test_cache_path_selection_overflows_exactly_where_testing_every_glob_does() -> None:
    # The matcher recurses once per '**' component, so a glob with about as
    # many components as the recursion limit overflows it, and validation
    # accepts such a glob. Where it overflows depends on how deep the stack
    # already is when the matcher runs, which the index must not change: find
    # the deepest glob the old method still decides, then compare both methods
    # on either side of it, with the deep glob alone, after a glob that
    # accepts first, before one, and after a glob the index skips.
    vcs = _listed(("target/file",))
    decided, overflowed = 1, sys.getrecursionlimit() + 20
    assert _every_glob_outcome(vcs, (_deep_glob(decided),)) == ("target/file",)
    assert _every_glob_outcome(vcs, (_deep_glob(overflowed),)) is None
    while overflowed - decided > 1:
        middle = (decided + overflowed) // 2
        if _every_glob_outcome(vcs, (_deep_glob(middle),)) is None:
            overflowed = middle
        else:
            decided = middle
    for count in range(decided - 2, overflowed + 3):
        deep = _deep_glob(count)
        for cache_globs in ((deep,), ("**", deep), (deep, "**"), ("src", deep)):
            assert _indexed_outcome(vcs, cache_globs) == _every_glob_outcome(
                vcs, cache_globs
            ), (count, cache_globs[0][:20])
    # A path the deep glob cannot contain never reaches it.
    source = _listed(("src/file",))
    deep = _deep_glob(overflowed + 20)
    assert _indexed_outcome(source, (deep, "**")) == ("src/file",)
    assert _every_glob_outcome(source, (deep, "**")) == ("src/file",)


Method = Callable[[cli._GitVcs, Sequence[str]], tuple[str, ...] | None]
_FRESH = itertools.count()


def _terminal_literal_outcome(
    method: Method,
    glob_templates: Sequence[str],
    listing_templates: Sequence[str],
    count: int,
) -> tuple[str, ...] | None:
    """Run ``method`` with ``{deep}`` a glob of ``count`` '**' components ending
    in ``{z}``, a literal no earlier run has used, so fnmatch has not compiled
    it yet; the result names it 'Z'."""

    literal = f"z{next(_FRESH):07d}"
    deep = "target/" + "**/" * count + literal
    vcs = _listed([template.format(z=literal) for template in listing_templates])
    outcome = method(
        vcs, [template.format(z=literal, deep=deep) for template in glob_templates]
    )
    if outcome is None:
        return None
    return tuple(path.replace(literal, "Z") for path in outcome)


def _last_decided(
    method: Method, glob_templates: Sequence[str], listing_templates: Sequence[str]
) -> int:
    decided, overflowed = 1, sys.getrecursionlimit() + 20
    assert _terminal_literal_outcome(method, glob_templates, listing_templates, decided)
    assert (
        _terminal_literal_outcome(method, glob_templates, listing_templates, overflowed)
        is None
    )
    while overflowed - decided > 1:
        middle = (decided + overflowed) // 2
        if (
            _terminal_literal_outcome(method, glob_templates, listing_templates, middle)
            is None
        ):
            overflowed = middle
        else:
            decided = middle
    return decided


COMPILING_CASES = (
    # Testing every glob compiles the literal while testing '{z}/**', which
    # the index skips for 'target/...', so the deep match finds it compiled.
    (("{z}/**", "{deep}"), ("target/{z}",)),
    # Here the deep match itself compiles it, at the bottom of its recursion.
    (("{deep}", "{z}/**"), ("target/{z}",)),
    # Only a path with at least as many components as the glob compiles it.
    (("{z}/*/q", "{deep}"), ("a", "b/c/d", "target/{z}")),
    # A path with no components compiles nothing.
    (("{z}/**", "{deep}"), ("/", "target/{z}")),
    # An all-literal glob is compared without fnmatch, so compiles nothing.
    (("{z}/q", "{z}/*/r", "{deep}"), ("a/b", "c/d/e", "target/{z}")),
    # A glob after the one that accepts a path is not tested against it.
    (("node_modules", "{z}/**", "{deep}"), ("node_modules/a", "target/{z}")),
)


@pytest.mark.parametrize(("glob_templates", "listing_templates"), COMPILING_CASES)
def test_cache_path_selection_compiles_patterns_where_testing_every_glob_does(
    glob_templates: tuple[str, ...], listing_templates: tuple[str, ...]
) -> None:
    # fnmatch compiles a pattern the first time it meets it, which takes about
    # a dozen frames, so a deep '**' match that meets an uncompiled literal
    # overflows earlier than one that finds it compiled. A test the index
    # skips can compile such a literal, so the index must leave the deepest
    # glob each method decides where it was.
    decided = _last_decided(_every_glob_outcome, glob_templates, listing_templates)
    assert _last_decided(_indexed_outcome, glob_templates, listing_templates) == decided
    for count in (decided - 1, decided, decided + 1):
        assert _terminal_literal_outcome(
            _indexed_outcome, glob_templates, listing_templates, count
        ) == _terminal_literal_outcome(
            _every_glob_outcome, glob_templates, listing_templates, count
        ), count


def test_cache_path_selection_keeps_what_fnmatch_evicts(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # fnmatch's pattern cache is bounded. Here the literal is compiled by the
    # first path, evicted by the patterns the second path compiles, and
    # compiled again, shallowly, only by the test of '{z}/**' against the
    # third path, a glob the index does not keep for that path. The test must
    # still be made. The cache is shrunk to 8 patterns, and
    # cleared before each run, so eviction is cheap to reach and every run
    # starts cold.
    compile_pattern = getattr(fnmatch, "_compile_pattern", None)
    uncached = getattr(compile_pattern, "__wrapped__", None)
    if uncached is None:
        pytest.skip("this fnmatch has no least-recently-used pattern cache")
    small = functools.lru_cache(maxsize=8, typed=True)(uncached)
    monkeypatch.setattr(fnmatch, "_compile_pattern", small)

    def cold(method: Method) -> Method:
        def run(vcs: cli._GitVcs, cache_globs: Sequence[str]) -> tuple[str, ...] | None:
            small.cache_clear()
            return method(vcs, cache_globs)

        return run

    glob_templates = (
        "{z}/**",
        *(f"compile-area/p{index}*" for index in range(8)),
        "{deep}",
    )
    listing_templates = ("src/file", "compile-area/x", "target/{z}")
    decided = _last_decided(cold(_every_glob_outcome), glob_templates, listing_templates)
    assert (
        _last_decided(cold(_indexed_outcome), glob_templates, listing_templates)
        == decided
    )
    for count in (decided - 1, decided, decided + 1):
        assert _terminal_literal_outcome(
            cold(_indexed_outcome), glob_templates, listing_templates, count
        ) == _terminal_literal_outcome(
            cold(_every_glob_outcome), glob_templates, listing_templates, count
        ), count


def _compiles(events: Sequence[tuple[str, ...]], size: int) -> tuple[tuple[str, ...], ...]:
    """The patterns an fnmatch cache holding ``size`` patterns compiles, in
    order, for the patterns ``events`` pass it, and what it holds at the end."""

    cache: dict[str, None] = {}
    compiled: list[str] = []
    for event in events:
        if event[0] != "fnmatch":
            continue
        if cache.pop(event[1], "absent") == "absent":
            compiled.append(event[1])
            if len(cache) >= size:
                del cache[next(iter(cache))]
        cache[event[1]] = None
    return tuple(compiled), tuple(cache)


def test_cache_path_selection_repeats_runs_fnmatch_cannot_hold(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Three skipped globs pass three different first components to fnmatch for
    # every source path. A cache of three keeps them all, so the second and
    # third source paths would only find them compiled; a cache of two evicts
    # one each time, so every source path compiles patterns again, and those
    # tests must still be made.
    compile_pattern = getattr(fnmatch, "_compile_pattern", None)
    uncached = getattr(compile_pattern, "__wrapped__", None)
    if uncached is None:
        pytest.skip("this fnmatch has no least-recently-used pattern cache")
    events = _spy_on_matcher_and_fnmatch(monkeypatch)
    cache_globs = ("a1/*", "a2/*", "a3/*", "target")
    vcs = _listed(("src/x", "src/y", "src/z", "target/q", "src/w"))
    made: dict[int, int] = {}
    for size in (2, 3):
        monkeypatch.setattr(
            fnmatch, "_compile_pattern", functools.lru_cache(maxsize=size, typed=True)(uncached)
        )
        events.clear()
        expected = _tracked_by_every_glob(vcs, LISTED_CHECKOUT, cache_globs)
        every_glob = _compiles(events, size)
        events.clear()
        assert vcs.tracked_cache_paths(LISTED_CHECKOUT, cache_globs) == expected
        assert _compiles(events, size) == every_glob, size
        made[size] = sum(event[0] == "test" for event in events)
    # Not vacuous: with room for every component, the repeated runs are left out.
    assert made[3] < made[2]


def _deepest_cold_caller(method: Callable[[], object]) -> int:
    """How deep a caller can recurse before calling ``method`` and still get a
    result, with fnmatch's, re's and the matcher's caches emptied first."""

    caches = (
        getattr(getattr(fnmatch, "_compile_pattern", None), "cache_clear", None),
        re.purge,
        cli._cache_glob_index.cache_clear,
        cli._glob_pattern_parts.cache_clear,
        cli._glob_parts_are_literal.cache_clear,
    )

    def descend(depth: int) -> object:
        return method() if depth == 0 else descend(depth - 1)

    def succeeds(depth: int) -> bool:
        for clear in caches:
            if callable(clear):
                clear()
        try:
            descend(depth)
        except RecursionError:
            return False
        return True

    low, high = 0, sys.getrecursionlimit()
    assert succeeds(low) and not succeeds(high)
    while high - low > 1:
        middle = (low + high) // 2
        if succeeds(middle):
            low = middle
        else:
            high = middle
    return low


@pytest.mark.parametrize(
    ("cache_globs", "listing"),
    [
        # A skipped glob compiles 'ignored' for the first path.
        (("ignored/**/target", "target"), ("src/x",)),
        # ... after a path that a kept glob accepted.
        (("target", "ignored/**/target"), ("target/x", "src/y")),
        # ... after a kept glob that compiled its own pattern.
        (("t*", "ignored/*"), ("src/x",)),
    ],
)
def test_cache_path_selection_compiles_at_the_same_stack_depth(
    cache_globs: tuple[str, ...], listing: tuple[str, ...]
) -> None:
    # Compiling a pattern from cold caches is the deepest thing either method
    # does here, so the skipped test that compiles 'ignored' must still be made
    # from where testing every glob made it: a caller one frame too deep for it
    # gets RecursionError from both methods or from neither.
    vcs = _listed(listing)
    every_glob = _deepest_cold_caller(
        lambda: _tracked_by_every_glob(vcs, LISTED_CHECKOUT, cache_globs)
    )
    assert _deepest_cold_caller(
        lambda: vcs.tracked_cache_paths(LISTED_CHECKOUT, cache_globs)
    ) == every_glob


def test_compiling_a_literal_first_moves_where_a_deep_glob_overflows() -> None:
    # Without this difference the test above could not tell the methods apart.
    compiled_first, compiled_deep = COMPILING_CASES[0], COMPILING_CASES[1]
    assert _last_decided(_every_glob_outcome, *compiled_first) > _last_decided(
        _every_glob_outcome, *compiled_deep
    )


def _git(checkout: Path, *arguments: str) -> None:
    subprocess.run(
        [
            "git", "-C", str(checkout), "-c", "user.name=fixture",
            "-c", "user.email=fixture@example.invalid", *arguments,
        ],
        check=True,
        capture_output=True,
    )


def test_tracked_cache_paths_is_unchanged_on_a_real_checkout(tmp_path: Path) -> None:
    checkout = tmp_path / "checkout"
    _git_checkout(checkout)
    for relative in (
        "src/main.rs",
        "src/target.rs",
        "targets/keep",
        "target/committed.o",
        "node_modules/pkg/index.js",
        "ignored/x/y/target/z",
        "ignored/x/target-debug/w",
        ".cache/blob",
        ".target/hidden",
        "a b/target/space",
        "Target/upper",
        "x]y/bracket",
    ):
        (checkout / relative).parent.mkdir(parents=True, exist_ok=True)
        (checkout / relative).write_bytes(relative.encode())
    _git(checkout, "add", "-A")
    _git(checkout, "commit", "-q", "-m", "tracked")
    # One path only in HEAD and one only in the index, so the listings differ
    # and the order of the union matters.
    _git(checkout, "rm", "-q", "--cached", "target/committed.o")
    (checkout / "target/staged.o").write_bytes(b"staged")
    _git(checkout, "add", "target/staged.o")

    tracked = cli._GitVcs().tracked_cache_paths(checkout, LIVE_SHAPED)
    assert tracked == (
        "ignored/x/target-debug/w",
        "ignored/x/y/target/z",
        "node_modules/pkg/index.js",
        "target/staged.o",
        "target/committed.o",
    )
    assert tracked == _tracked_by_every_glob(cli._GitVcs(), checkout, LIVE_SHAPED)
    for cache_globs in (TRICKY_GLOBS, *((pattern,) for pattern in TRICKY_GLOBS)):
        assert cli._GitVcs().tracked_cache_paths(
            checkout, cache_globs
        ) == _tracked_by_every_glob(cli._GitVcs(), checkout, cache_globs), cache_globs

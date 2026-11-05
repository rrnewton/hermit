#!/usr/bin/env python3
"""Run the explicitly inventoried Python tests for one repository component.

The manifest is data rather than filename folklore: every Python file matching pytest's default
``test_*.py`` or ``*_test.py`` patterns is named exactly once, except suites that are explicitly
delegated to a separate validation lane. This
keeps a newly added or accidentally duplicated test from silently falling out of selective runs.

Examples::

    python3 scripts/run_component_tests.py --self-test
    python3 scripts/run_component_tests.py --list-components
    python3 scripts/run_component_tests.py --component planner -- -q
    python3 scripts/run_component_tests.py --component dagrun --shard 2/6 -- -q

Pytest always runs from ``py/`` with that workspace's configuration.  Arguments after ``--`` are
passed to pytest before the manifest's explicit file list.  A shard uses
``scripts/run_pytest_shard.py`` so parameterized cases from one test function remain together.

This module is also loaded into that pytest as a plugin, which prints each failing test's node ID
the moment it fails.  Pytest's own list of failures comes only in its final summary, so a suite
killed at its timeout used to leave nothing but an ``F`` in a row of dots.
"""

from __future__ import annotations

import argparse
import json
import shlex
import os
import signal
import subprocess
import sys
import tempfile
import time
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pytest


REPO_ROOT = Path(__file__).resolve().parent.parent
PY_ROOT = REPO_ROOT / "py"
SCRIPTS_ROOT = Path(__file__).resolve().parent
#: Loaded with ``-p``; the shard runner already has this directory on ``sys.path``.
FAILURE_IDENTITY_PLUGIN = Path(__file__).stem
#: ``python -m pytest``, plus this directory at the END of ``sys.path`` so the plugin can be
#: imported without shadowing anything the tests import, and without changing the environment
#: the tests inherit.  ``-c`` would leave ``''`` first on ``sys.path``, which follows a test's
#: ``chdir``; ``-m`` puts the absolute starting directory there, and so does this.
_PYTEST_WITH_SCRIPTS = (
    "import os, sys; sys.path[0] = os.getcwd(); sys.path.append(sys.argv.pop(1)); "
    "import pytest; raise SystemExit(pytest.main(sys.argv[1:]))"
)
FAILURE_IDENTITY_PREFIX = "component-tests: FAILED"


def pytest_runtest_logreport(report: pytest.TestReport) -> None:
    """Name a failing test now, not in a summary a timeout may never let pytest print."""

    if report.failed:
        # The newline ends a row of progress dots when stderr shares a log with stdout.
        print(
            f"\n{FAILURE_IDENTITY_PREFIX} {report.nodeid} ({report.when})",
            file=sys.stderr,
            flush=True,
        )
DEFAULT_MANIFEST = REPO_ROOT / "validation" / "components.json"
EXPECTED_COMPONENTS = frozenset(
    {
        "agentctl",
        "dagrun",
        "experiment-runner",
        "herdr-run",
        "planner",
        "repository-infrastructure",
        "tick-hub",
        "wrkslots",
        "wrkviz",
    }
)


class ManifestError(ValueError):
    """The component manifest is malformed or does not cover the test inventory."""


@dataclass(frozen=True)
class Component:
    """The source boundary, downstream consumers, and owned test files for one component."""

    source_prefixes: tuple[str, ...]
    reverse_dependencies: tuple[str, ...]
    test_files: tuple[str, ...]


@dataclass(frozen=True)
class Manifest:
    """A validated component manifest."""

    components: Mapping[str, Component]
    separate_test_files: Mapping[str, str]


def _object_mapping(value: object, where: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ManifestError(f"{where} must be an object")
    result: dict[str, object] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise ManifestError(f"{where} has a non-string key")
        result[key] = item
    return result


def _string_list(value: object, where: str, *, allow_empty: bool) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise ManifestError(f"{where} must be a list of strings")
    result = tuple(item for item in value if isinstance(item, str))
    if not allow_empty and not result:
        raise ManifestError(f"{where} must not be empty")
    if len(result) != len(set(result)):
        raise ManifestError(f"{where} contains a duplicate entry")
    if result != tuple(sorted(result)):
        raise ManifestError(f"{where} must be sorted")
    return result


def _repo_relative_path(value: str, where: str, *, directory_allowed: bool) -> str:
    path = value[:-1] if directory_allowed and value.endswith("/") else value
    if (
        not path
        or path.startswith("/")
        or "\\" in path
        or "//" in path
        or any(part in {"", ".", ".."} for part in path.split("/"))
    ):
        raise ManifestError(f"{where} is not a normalized repository-relative path: {value!r}")
    return value


def _component(value: object, name: str) -> Component:
    raw = _object_mapping(value, f"components.{name}")
    expected = {"source_prefixes", "reverse_dependencies", "test_files"}
    if set(raw) != expected:
        raise ManifestError(
            f"components.{name} keys must be exactly {sorted(expected)}; found {sorted(raw)}"
        )
    prefixes = _string_list(
        raw["source_prefixes"], f"components.{name}.source_prefixes", allow_empty=False
    )
    dependencies = _string_list(
        raw["reverse_dependencies"],
        f"components.{name}.reverse_dependencies",
        allow_empty=True,
    )
    tests = _string_list(raw["test_files"], f"components.{name}.test_files", allow_empty=False)
    for index, prefix in enumerate(prefixes):
        _repo_relative_path(
            prefix,
            f"components.{name}.source_prefixes[{index}]",
            directory_allowed=True,
        )
    for index, test_file in enumerate(tests):
        _repo_relative_path(
            test_file,
            f"components.{name}.test_files[{index}]",
            directory_allowed=False,
        )
        test_path = Path(test_file)
        if test_path.parts[:1] != ("py",) or not _is_pytest_file(test_path):
            raise ManifestError(
                f"components.{name}.test_files[{index}] does not match pytest's Python test "
                "filename patterns:"
                f" {test_file!r}"
            )
        if test_path.suffix != ".py":
            raise ManifestError(
                f"components.{name}.test_files[{index}] is not a Python file: {test_file!r}"
            )
    return Component(prefixes, dependencies, tests)


def _validate_reverse_dependencies(components: Mapping[str, Component]) -> None:
    for name, component in components.items():
        unknown = set(component.reverse_dependencies) - set(components)
        if unknown:
            raise ManifestError(
                f"components.{name}.reverse_dependencies names unknown components:"
                f" {sorted(unknown)}"
            )
        if name in component.reverse_dependencies:
            raise ManifestError(f"components.{name} depends on itself")

    visiting: list[str] = []
    complete: set[str] = set()

    def visit(name: str) -> None:
        if name in complete:
            return
        if name in visiting:
            cycle = visiting[visiting.index(name) :] + [name]
            raise ManifestError(f"reverse dependency cycle: {' -> '.join(cycle)}")
        visiting.append(name)
        for dependent in components[name].reverse_dependencies:
            visit(dependent)
        visiting.pop()
        complete.add(name)

    for component_name in sorted(components):
        visit(component_name)


def _load_manifest(path: Path) -> Manifest:
    try:
        raw_value: object = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ManifestError(f"cannot read {path}: {error}") from error
    raw = _object_mapping(raw_value, "manifest")
    expected = {"schema_version", "components", "separate_test_files"}
    if set(raw) != expected:
        raise ManifestError(
            f"manifest keys must be exactly {sorted(expected)}; found {sorted(raw)}"
        )
    version = raw["schema_version"]
    if not isinstance(version, int) or isinstance(version, bool) or version != 1:
        raise ManifestError("manifest.schema_version must be 1")

    raw_components = _object_mapping(raw["components"], "manifest.components")
    if set(raw_components) != EXPECTED_COMPONENTS:
        missing = sorted(EXPECTED_COMPONENTS - set(raw_components))
        extra = sorted(set(raw_components) - EXPECTED_COMPONENTS)
        raise ManifestError(f"manifest component mismatch: missing={missing}, extra={extra}")
    if tuple(raw_components) != tuple(sorted(raw_components)):
        raise ManifestError("manifest.components must be sorted by component name")
    components = {
        name: _component(raw_components[name], name) for name in sorted(raw_components)
    }

    raw_separate = _object_mapping(raw["separate_test_files"], "manifest.separate_test_files")
    separate: dict[str, str] = {}
    for test_file, suite_value in raw_separate.items():
        _repo_relative_path(
            test_file, "manifest.separate_test_files key", directory_allowed=False
        )
        if not isinstance(suite_value, str) or not suite_value:
            raise ManifestError(
                f"manifest.separate_test_files[{test_file!r}] must name its validation suite"
            )
        separate[test_file] = suite_value
    if tuple(raw_separate) != tuple(sorted(raw_separate)):
        raise ManifestError("manifest.separate_test_files must be sorted")

    _validate_reverse_dependencies(components)
    prefixes: dict[str, str] = {}
    for name, component in components.items():
        for prefix in component.source_prefixes:
            previous = prefixes.setdefault(prefix, name)
            if previous != name:
                raise ManifestError(
                    f"source prefix {prefix!r} is assigned to both {previous} and {name}"
                )
            target = REPO_ROOT / prefix.rstrip("/")
            if not target.exists():
                raise ManifestError(f"components.{name} source prefix does not exist: {prefix}")

    manifest = Manifest(components=components, separate_test_files=separate)
    problems = _inventory_problems(manifest, _discover_test_files())
    if problems:
        raise ManifestError("\n".join(problems))
    return manifest


def load_manifest(path: Path = DEFAULT_MANIFEST) -> Manifest:
    """Load the checked-in manifest after validating its complete test inventory."""

    return _load_manifest(path.resolve())


def _source_prefix_matches(path: str, prefix: str) -> bool:
    """Match a declared directory prefix or an exact-file component boundary."""

    return path.startswith(prefix) if prefix.endswith("/") else path == prefix


def components_for_paths(
    paths: Sequence[str], manifest: Manifest
) -> tuple[frozenset[str], dict[str, str]]:
    """Return owning components plus their transitive reverse-dependency closure.

    Paths outside this manifest's component boundary are deliberately absent from the result;
    the repository validation planner decides whether they are metadata, shared infrastructure,
    or unknown and therefore require the complete contract.
    """

    selected: set[str] = set()
    because: dict[str, str] = {}
    test_owners = {
        test_file: name
        for name, component in manifest.components.items()
        for test_file in component.test_files
    }
    for path in paths:
        owner = test_owners.get(path)
        if owner is not None:
            selected.add(owner)
            because.setdefault(owner, path)
            continue
        matches = {
            name
            for name, component in manifest.components.items()
            if any(
                _source_prefix_matches(path, prefix)
                for prefix in component.source_prefixes
            )
        }
        for name in matches:
            selected.add(name)
            because.setdefault(name, path)

    pending = list(selected)
    while pending:
        name = pending.pop()
        for dependent in manifest.components[name].reverse_dependencies:
            if dependent not in selected:
                selected.add(dependent)
                because[dependent] = f"reverse dependency of {name}"
                pending.append(dependent)
    return frozenset(selected), because


def _is_pytest_file(path: Path) -> bool:
    """Match pytest's default Python module patterns exactly."""

    return path.suffix == ".py" and (
        path.name.startswith("test_") or path.name.endswith("_test.py")
    )


def _discover_test_files(repo_root: Path = REPO_ROOT) -> frozenset[str]:
    # Include untracked source files so adding a test turns the guard red before the author stages
    # it, while respecting .gitignore so caches, virtualenvs, and vendored node_modules do not
    # become part of the repository's test contract by accident.
    try:
        result = subprocess.run(
            [
                "git",
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                "py",
            ],
            cwd=repo_root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise ManifestError(f"cannot inventory Python tests with git: {error}") from error
    if result.returncode != 0:
        detail = result.stderr.strip() or f"exit {result.returncode}"
        raise ManifestError(f"cannot inventory Python tests with git: {detail}")
    return frozenset(
        path
        for path in result.stdout.split("\0")
        if path
        and Path(path).parts[:1] == ("py",)
        and _is_pytest_file(Path(path))
        and (repo_root / path).is_file()
    )


def _inventory_problems(manifest: Manifest, discovered: frozenset[str]) -> list[str]:
    owners: dict[str, list[str]] = {}
    for name, component in manifest.components.items():
        for test_file in component.test_files:
            owners.setdefault(test_file, []).append(name)

    problems: list[str] = []
    for test_file, names in sorted(owners.items()):
        if len(names) > 1:
            problems.append(
                f"duplicate Python test ownership: {test_file} appears in {', '.join(names)}"
            )
    overlap = set(owners).intersection(manifest.separate_test_files)
    for test_file in sorted(overlap):
        problems.append(
            f"duplicate Python test ownership: {test_file} is both component-owned and separate"
        )

    classified = set(owners).union(manifest.separate_test_files)
    for test_file in sorted(discovered - classified):
        problems.append(f"unclassified Python test file: {test_file}")
    for test_file in sorted(classified - discovered):
        problems.append(f"manifest names a missing Python test file: {test_file}")
    return problems


def _exercise_inventory_guards(manifest: Manifest, discovered: frozenset[str]) -> None:
    """Mutation controls prove that both required fail-closed checks can turn red."""
    if not _is_pytest_file(Path("test_prefix.py")) or not _is_pytest_file(
        Path("suffix_test.py")
    ):
        raise AssertionError("pytest filename-pattern guard does not cover both defaults")
    if _is_pytest_file(Path("helper.py")):
        raise AssertionError("pytest filename-pattern guard classifies an ordinary helper")

    with tempfile.TemporaryDirectory(prefix="agent-utils-test-inventory-") as raw:
        repository = Path(raw)
        tracked = repository / "py/tests/test_removed.py"
        untracked = repository / "py/tests/new_test.py"
        tracked.parent.mkdir(parents=True)
        tracked.write_text("def test_old(): pass\n", encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=repository, check=True)
        subprocess.run(
            ["git", "add", "py/tests/test_removed.py"], cwd=repository, check=True
        )
        tracked.unlink()
        untracked.write_text("def test_new(): pass\n", encoding="utf-8")
        actual = _discover_test_files(repository)
        if "py/tests/test_removed.py" in actual:
            raise AssertionError("a deleted tracked test remained in the live inventory")
        if "py/tests/new_test.py" not in actual:
            raise AssertionError("an untracked live test was absent from the inventory")

    first_name = sorted(manifest.components)[0]
    first_component = manifest.components[first_name]
    first_test = first_component.test_files[0]

    omitted_components = dict(manifest.components)
    omitted_components[first_name] = replace(
        first_component, test_files=first_component.test_files[1:]
    )
    omitted = Manifest(omitted_components, manifest.separate_test_files)
    if not any(
        problem == f"unclassified Python test file: {first_test}"
        for problem in _inventory_problems(omitted, discovered)
    ):
        raise AssertionError("unclassified-test mutation did not fail closed")

    second_name = next(name for name in sorted(manifest.components) if name != first_name)
    duplicate_components = dict(manifest.components)
    second_component = duplicate_components[second_name]
    duplicate_components[second_name] = replace(
        second_component, test_files=second_component.test_files + (first_test,)
    )
    duplicated = Manifest(duplicate_components, manifest.separate_test_files)
    if not any(
        problem.startswith(f"duplicate Python test ownership: {first_test}")
        for problem in _inventory_problems(duplicated, discovered)
    ):
        raise AssertionError("duplicate-test mutation did not fail closed")

    for paths, expected in (
        (("py/dagrun/io.py",), {"dagrun", "experiment-runner"}),
        (("rs/dagrun/src/io.rs",), {"dagrun", "experiment-runner"}),
        (("py/tests/test_tickhub_cli.py",), {"tick-hub"}),
        (("brand-new-tool/source.py",), set()),
    ):
        actual, _because = components_for_paths(paths, manifest)
        if set(actual) != expected:
            raise AssertionError(
                f"component source routing mismatch for {paths}: "
                f"expected {sorted(expected)}, got {sorted(actual)}"
            )


def _parse_shard(value: str) -> str:
    try:
        raw_index, raw_count = value.split("/", 1)
        index, count = int(raw_index), int(raw_count)
    except (TypeError, ValueError) as error:
        raise argparse.ArgumentTypeError("shard must have the form INDEX/COUNT") from error
    if count < 1 or index < 0 or index >= count:
        raise argparse.ArgumentTypeError("shard requires COUNT > 0 and 0 <= INDEX < COUNT")
    return f"{index}/{count}"


def _pytest_command(pytest_args: Sequence[str], shard: str | None) -> list[str]:
    base_args = ["-p", FAILURE_IDENTITY_PLUGIN, *pytest_args]
    if shard is None:
        return [sys.executable, "-c", _PYTEST_WITH_SCRIPTS, str(SCRIPTS_ROOT), *base_args]
    return [
        sys.executable,
        str(SCRIPTS_ROOT / "run_pytest_shard.py"),
        "--shard",
        shard,
        "--",
        *base_args,
    ]


def _failure_identity_before_kill(*, plugin: bool) -> str:
    """Run a suite that fails and then hangs; kill it; return everything it printed.

    The hanging test announces itself through a file, and pytest reports one test before it
    sets up the next, so by the time the file exists the failure has been reported -- the
    kill needs no timing guess.
    """

    with tempfile.TemporaryDirectory(prefix="agent-utils-failure-identity-") as raw:
        root = Path(raw)
        started = root / "hang-started"
        (root / "test_identity.py").write_text(
            "import pathlib, time\n"
            "def test_fails_first():\n"
            "    assert False\n"
            "def test_then_hangs():\n"
            f"    pathlib.Path({str(started)!r}).touch()\n"
            "    time.sleep(120)\n",
            encoding="utf-8",
        )
        (root / "pytest.ini").write_text("[pytest]\n", encoding="utf-8")
        command = _pytest_command(["-q", "-p", "no:cacheprovider", "test_identity.py"], None)
        if not plugin:
            command.remove("-p")
            command.remove(FAILURE_IDENTITY_PLUGIN)
        output = root / "output"
        with output.open("wb") as sink:
            process = subprocess.Popen(
                command, cwd=root, stdout=sink, stderr=subprocess.STDOUT, start_new_session=True
            )
            try:
                deadline = time.monotonic() + 45
                while not started.exists():
                    if process.poll() is not None:
                        raise AssertionError(
                            "failure-identity fixture exited before hanging: "
                            + output.read_text(encoding="utf-8", errors="replace")
                        )
                    if time.monotonic() > deadline:
                        raise AssertionError("failure-identity fixture never reached its hang")
                    time.sleep(0.05)
            finally:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        return output.read_text(encoding="utf-8", errors="replace")


def _exercise_failure_identity() -> None:
    """A failing node ID must survive the suite being killed before its summary."""

    expected = f"{FAILURE_IDENTITY_PREFIX} test_identity.py::test_fails_first (call)"
    reported = _failure_identity_before_kill(plugin=True)
    if expected not in reported:
        raise AssertionError(f"failing node ID was not printed before the kill: {reported!r}")
    # The control: pytest alone names nothing before its summary, so the check above can fail.
    unreported = _failure_identity_before_kill(plugin=False)
    if "test_fails_first" in unreported:
        raise AssertionError(f"control without the plugin named the failure: {unreported!r}")


def _run_component(
    name: str, component: Component, shard: str | None, pytest_args: Sequence[str]
) -> int:
    test_files = [str((REPO_ROOT / path).relative_to(PY_ROOT)) for path in component.test_files]
    command = _pytest_command(
        ["-c", "pyproject.toml", "--rootdir=.", *pytest_args, *test_files], shard
    )

    print(
        f"component-tests: {name}: files={len(test_files)}, shard={shard or 'all'}",
        flush=True,
    )
    print(f"component-tests: {shlex.join(command)}", flush=True)
    return subprocess.run(command, cwd=PY_ROOT, check=False).returncode


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=Path,
        default=DEFAULT_MANIFEST,
        help=f"component manifest (default: {DEFAULT_MANIFEST.relative_to(REPO_ROOT)})",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--component", help="run this named component's explicit test files")
    mode.add_argument("--list-components", action="store_true", help="list components and exit")
    mode.add_argument("--self-test", action="store_true", help="validate coverage and guard logic")
    parser.add_argument(
        "--shard",
        type=_parse_shard,
        metavar="INDEX/COUNT",
        help="run one deterministic, family-preserving shard",
    )
    parser.add_argument("pytest_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)

    try:
        manifest = load_manifest(args.manifest)
    except ManifestError as error:
        parser.error(str(error))

    pytest_args = list(args.pytest_args)
    if pytest_args[:1] == ["--"]:
        pytest_args = pytest_args[1:]
    if args.self_test:
        if args.shard is not None or pytest_args:
            parser.error("--self-test does not accept --shard or pytest arguments")
        discovered = _discover_test_files()
        _exercise_inventory_guards(manifest, discovered)
        _exercise_failure_identity()
        mapped = sum(len(component.test_files) for component in manifest.components.values())
        print(
            "run_component_tests --self-test: PASSED "
            f"({mapped} component tests, {len(manifest.separate_test_files)} separate suite)"
        )
        return 0
    if args.list_components:
        if args.shard is not None or pytest_args:
            parser.error("--list-components does not accept --shard or pytest arguments")
        for name, component in manifest.components.items():
            reverse = ",".join(component.reverse_dependencies) or "-"
            print(f"{name}\t{len(component.test_files)}\treverse-dependencies={reverse}")
        return 0

    assert args.component is not None
    selected_component = manifest.components.get(args.component)
    if selected_component is None:
        parser.error(
            f"unknown component {args.component!r}; choose from {', '.join(manifest.components)}"
        )
    return _run_component(args.component, selected_component, args.shard, pytest_args)


if __name__ == "__main__":
    raise SystemExit(main())

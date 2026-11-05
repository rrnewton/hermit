"""Structural guards for the composed repository validation graph."""

from __future__ import annotations

import importlib.util
import os
import shlex
import subprocess
import sys
from collections import Counter
from pathlib import Path
from types import ModuleType

import pytest

from dagrun.io import dag_from_path
from dagrun.scheduler import cap_config_max_cpus


SCRIPTS = Path(__file__).resolve().parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))

from validate import GROUP_LABELS, selection_for  # noqa: E402
from dagrun.cli import _select_steps_by_labels  # noqa: E402


REPO_ROOT = Path(__file__).resolve().parents[2]
DOCS_STEP = "repository.docs.embedded-userguides"
RUST_BUILD_STEP = "repository.build.rust-launchers"


def _example_checker() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "_dagrun_example_checker_under_test", SCRIPTS / "check_dagrun_examples.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _listed(command: str) -> set[str]:
    completed = subprocess.run(
        [sys.executable, *command.split()],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    return {line.split(" ", 1)[0] for line in completed.stdout.splitlines() if line}


def _flag_values(command: str, flag: str) -> list[str]:
    tokens = shlex.split(command)
    return [tokens[index + 1] for index, token in enumerate(tokens[:-1]) if token == flag]


def _workspace_crates() -> set[str]:
    workspace = (REPO_ROOT / "rs/Cargo.toml").read_text(encoding="utf-8")
    members_text = workspace.split("members = [", 1)[1].split("]", 1)[0]
    members = [line.strip().strip('",') for line in members_text.splitlines() if '"' in line]
    names: set[str] = set()
    for member in members:
        manifest = (REPO_ROOT / "rs" / member / "Cargo.toml").read_text(encoding="utf-8")
        package = manifest.split("[package]", 1)[1].split("[", 1)[0]
        name_line = next(line for line in package.splitlines() if line.startswith("name = "))
        names.add(name_line.split('"', 2)[1])
    return names


def test_package_nodes_cover_every_artifact_once_after_generated_docs() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    python_steps = [
        step for step in config.steps if "scripts/check_python_packages.py" in step.cmd
    ]
    rust_steps = [
        step for step in config.steps if "scripts/check_rust_packages.py" in step.cmd
    ]

    python_projects = Counter(
        project for step in python_steps for project in _flag_values(step.cmd, "--project")
    )
    rust_crates = Counter(
        crate for step in rust_steps for crate in _flag_values(step.cmd, "--crate")
    )

    assert python_projects == Counter(
        {name: 1 for name in _listed("scripts/check_python_packages.py --list-projects")}
    )
    assert rust_crates == Counter(
        {name: 1 for name in _listed("scripts/check_rust_packages.py --list-crates")}
    )
    assert all(DOCS_STEP in step.deps for step in python_steps)
    assert all(RUST_BUILD_STEP in step.deps for step in rust_steps)
    build = next(step for step in config.steps if step.tag == RUST_BUILD_STEP)
    assert DOCS_STEP in build.deps


def test_javascript_validation_entrypoints_are_owned_once() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    commands = "\n".join(step.cmd for step in config.steps)

    # Playwright discovers every *.spec.js file. Every standalone *.test.cjs benchmark is instead
    # required to occur exactly once in a graph command, so adding a third cannot pass silently.
    playwright = (REPO_ROOT / "py/tests/js/browser/playwright.config.cjs").read_text(
        encoding="utf-8"
    )
    assert "*.spec.js" in playwright
    assert list((REPO_ROOT / "py/tests/js/browser").glob("*.spec.js"))
    benchmark_suites = sorted(
        (REPO_ROOT / "py/tests/js/browser").glob("*.test.cjs")
    )
    assert benchmark_suites
    for path in benchmark_suites:
        suite = path.name
        assert commands.count(suite) == 1

    # The vibe page suites are owned by their Rust cargo-test harnesses. Running them directly in
    # the graph as well would double their several-hundred-case cost.
    harnesses = [
        path.read_text(encoding="utf-8")
        for path in sorted((REPO_ROOT / "vibe-talk/tests").glob("*.rs"))
    ]
    vibe_suites = sorted((REPO_ROOT / "vibe-talk/tests/js").glob("*.test.mjs"))
    assert vibe_suites
    for path in vibe_suites:
        suite = path.name
        assert suite not in commands
        assert sum(suite in harness for harness in harnesses) == 1


def test_rust_and_cross_nodes_cover_their_registries_once() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    rust_tests = Counter(
        crate
        for step in config.steps
        if step.tag.startswith("rust.")
        for crate in _flag_values(step.cmd, "-p")
    )
    cross_tools = Counter(
        tool
        for step in config.steps
        if step.tag.startswith("cross.")
        for tool in _flag_values(step.cmd, "--tool")
    )

    assert rust_tests == Counter({name: 1 for name in _workspace_crates()})
    assert cross_tools == Counter(
        {name: 1 for name in _listed("cross/differential.py --list-tools")}
    )


def test_wrkslots_component_selection_includes_every_lifecycle_shard() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    lifecycle = [
        step for step in config.steps if step.tag.startswith("python-lifecycle.")
    ]

    assert len(lifecycle) == 9
    assert all("component-wrkslots" in step.labels for step in lifecycle)


def test_every_example_is_validated_under_both_dagrun_engines() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    example_steps = [
        step for step in config.steps if "scripts/check_dagrun_examples.py" in step.cmd
    ]

    assert Counter(
        engine for step in example_steps for engine in _flag_values(step.cmd, "--engine")
    ) == Counter({"python": 1, "rust": 1})
    assert all(step.delegated_children for step in example_steps)
    assert all(step.jobs_env == "AGENT_UTILS_VALIDATION_JOBS" for step in example_steps)

    differential = config.by_tag()["cross.dagrun.differential"]
    assert differential.jobs_env == "AGENT_UTILS_VALIDATION_JOBS"
    assert differential.hint.preferred_inner_jobs == 8
    assert all(step.hint.preferred_inner_jobs == 8 for step in example_steps)


def test_full_validation_graph_can_be_honestly_capped_to_four_cpus() -> None:
    capped = cap_config_max_cpus(dag_from_path(REPO_ROOT / "validation.dag.yaml"), 4)
    oversized = [
        (step.tag, step.hint.preferred_inner_jobs)
        for step in capped.steps
        if step.skip_reason is None
        and step.hint.preferred_inner_jobs is not None
        and step.hint.preferred_inner_jobs > 4
    ]
    assert oversized == []


def test_fixed_width_example_has_the_documented_narrow_runner_refusal() -> None:
    environment = os.environ.copy()
    python_path = str(REPO_ROOT / "py")
    inherited = environment.get("PYTHONPATH")
    environment["PYTHONPATH"] = (
        os.pathsep.join((python_path, inherited)) if inherited else python_path
    )
    completed = subprocess.run(
        [
            sys.executable,
            "-m",
            "dagrun",
            "run",
            "--dag",
            "examples/05-inner-jobs.json",
            "--max-cpus",
            "4",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
        ],
        cwd=REPO_ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 2
    assert "cannot lower guest parallelism" in completed.stderr
    assert "build.app (preferred_inner_jobs=8)" in completed.stderr


def test_example_checker_accepts_the_documented_narrow_refusal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    checker = _example_checker()
    example = tmp_path / "05-inner-jobs.json"
    example.write_text("{}", encoding="utf-8")
    calls: list[tuple[list[str], dict[str, object]]] = []
    profile_widths: list[int] = []

    def fake_run(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append((argv, kwargs))
        return subprocess.CompletedProcess(
            argv,
            2,
            stdout="",
            stderr=(
                "dagrun: run: --max-cpus 4 cannot lower guest parallelism for step(s) "
                "that offer no width channel: build.app (preferred_inner_jobs=8)"
            ),
        )

    def fake_profile(
        _command: list[str], _environment: dict[str, str], *, validation_jobs: int
    ) -> None:
        profile_widths.append(validation_jobs)

    monkeypatch.setenv("AGENT_UTILS_VALIDATION_JOBS", "4")
    monkeypatch.setattr(checker, "_effective_validation_jobs", lambda: 16)
    monkeypatch.setattr(checker, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(checker, "_examples", lambda: [example])
    monkeypatch.setattr(checker, "_engine", lambda _name: (["fake-dagrun"], {}))
    monkeypatch.setattr(checker.subprocess, "run", fake_run)
    monkeypatch.setattr(checker, "_profile_smoke", fake_profile)

    assert checker.main(["--engine", "python"]) == 0
    assert calls[0][0][-2:] == ["--max-cpus", "4"]
    assert calls[0][1]["capture_output"] is True
    assert profile_widths == [4]


def test_example_checker_runs_fixed_width_example_at_full_strength(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    checker = _example_checker()
    example = tmp_path / "05-inner-jobs.json"
    example.write_text("{}", encoding="utf-8")
    calls: list[tuple[list[str], dict[str, object]]] = []

    def fake_run(argv: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
        calls.append((argv, kwargs))
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setenv("AGENT_UTILS_VALIDATION_JOBS", "8")
    monkeypatch.setattr(checker, "_effective_validation_jobs", lambda: 16)
    monkeypatch.setattr(checker, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(checker, "_examples", lambda: [example])
    monkeypatch.setattr(checker, "_engine", lambda _name: (["fake-dagrun"], {}))
    monkeypatch.setattr(checker.subprocess, "run", fake_run)
    monkeypatch.setattr(checker, "_profile_smoke", lambda *args, **kwargs: None)

    assert checker.main(["--engine", "rust"]) == 0
    assert calls[0][0][-2:] == ["--max-cpus", "8"]
    assert calls[0][1]["capture_output"] is False


def test_example_checker_rejects_a_post_spawn_fixed_width_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    checker = _example_checker()
    example = tmp_path / "05-inner-jobs.json"
    example.write_text("{}", encoding="utf-8")

    def fake_run(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(
            argv,
            2,
            stdout="[build] make -j8 build\n",
            stderr=(
                "dagrun: run: --max-cpus 4 cannot lower guest parallelism for step(s) "
                "that offer no width channel: build.app (preferred_inner_jobs=8)"
            ),
        )

    monkeypatch.setenv("AGENT_UTILS_VALIDATION_JOBS", "4")
    monkeypatch.setattr(checker, "_effective_validation_jobs", lambda: 16)
    monkeypatch.setattr(checker, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(checker, "_examples", lambda: [example])
    monkeypatch.setattr(checker, "_engine", lambda _name: (["fake-dagrun"], {}))
    monkeypatch.setattr(checker.subprocess, "run", fake_run)

    assert checker.main(["--engine", "python"]) == 1


def test_example_checker_width_parser_is_strict_and_effective_cap_aware() -> None:
    checker = _example_checker()
    assert checker._validation_jobs({}, effective_jobs=4) == 4
    assert checker._validation_jobs(
        {"AGENT_UTILS_VALIDATION_JOBS": "3"}, effective_jobs=4
    ) == 3
    with pytest.raises(ValueError, match="must be a positive integer"):
        checker._validation_jobs(
            {"AGENT_UTILS_VALIDATION_JOBS": "+4"}, effective_jobs=16
        )


def test_example_checker_strips_its_width_control_from_nested_engines(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    checker = _example_checker()
    monkeypatch.setenv("AGENT_UTILS_VALIDATION_JOBS", "4")
    _command, environment = checker._engine("rust")
    assert "AGENT_UTILS_VALIDATION_JOBS" not in environment


def _selected_tags_for_paths(paths: list[str]) -> set[str]:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    groups, _because, components, _component_because = selection_for(paths)
    labels = [GROUP_LABELS[group] for group in groups]
    labels.extend(f"component-{component}" for component in components)
    selected = _select_steps_by_labels(config, labels)
    return {step.tag for step in selected.steps}


def test_examples_are_selected_by_their_sources_and_workspace_manifests() -> None:
    example_tags = {"examples.run.python", "examples.run.rust"}

    assert example_tags <= _selected_tags_for_paths(["examples/05-inner-jobs.json"])
    assert example_tags <= _selected_tags_for_paths(["rs/Cargo.lock"])
    assert example_tags.isdisjoint(_selected_tags_for_paths(["py/tick_hub/cli.py"]))


def test_wrkviz_source_selection_reaches_real_browser_gates() -> None:
    browser_tags = {
        "timeline.browser.functional",
        "timeline.browser.benchmark-instrument",
        "timeline.browser.benchmark",
    }

    assert browser_tags <= _selected_tags_for_paths(["py/wrkviz/synthetic.py"])
    assert browser_tags.isdisjoint(_selected_tags_for_paths(["py/tick_hub/cli.py"]))


def test_python_source_selection_reaches_cross_package_cli_surface() -> None:
    selected = _selected_tags_for_paths(["py/tick_hub/cli.py"])

    assert "python.repository-infrastructure.cli-surface" in selected
    assert "python.repository-infrastructure.test" not in selected


def test_cross_cutting_python_contracts_follow_every_observed_input() -> None:
    by_tag = dag_from_path(REPO_ROOT / "validation.dag.yaml").by_tag()
    expected_commands = {
        "python.repository-infrastructure.cli-no-optional-deps": (
            "test_cli_smoke_no_optional_deps.py"
        ),
        "python.repository-infrastructure.cli-surface": "test_cli_surface.py",
        "python.repository-infrastructure.cross-environment": (
            "test_cross_env_is_hermetic.py"
        ),
        "python.repository-infrastructure.cross-skip-accounting": (
            "test_cross_skip_accounting.py"
        ),
        "python.repository-infrastructure.dispatch-wiring": "test_repo_dispatch_wiring.py",
        "python.repository-infrastructure.package-contract": (
            "test_packaging_infrastructure.py"
        ),
        "python.repository-infrastructure.validation-graph": (
            "test_validation_graph_contract.py"
        ),
    }
    for tag, filename in expected_commands.items():
        assert filename in by_tag[tag].cmd

    python_tool = _selected_tags_for_paths(["py/tick_hub/cli.py"])
    assert {
        "python.repository-infrastructure.cli-no-optional-deps",
        "python.repository-infrastructure.cli-surface",
        "python.repository-infrastructure.dispatch-wiring",
        "python.repository-infrastructure.package-contract",
    } <= python_tool

    assert "python.repository-infrastructure.dispatch-wiring" in _selected_tags_for_paths(
        ["rs/tick-hub/src/lib.rs"]
    )
    assert "python.repository-infrastructure.package-contract" in _selected_tags_for_paths(
        ["py/wrkslots/tests/test_lifecycle.py"]
    )
    assert "python.repository-infrastructure.validation-graph" in _selected_tags_for_paths(
        ["vibe-talk/tests.dag.yaml"]
    )
    cross_harness = _selected_tags_for_paths(["cross/agentctl_differential.py"])
    assert {
        "python.repository-infrastructure.cross-environment",
        "python.repository-infrastructure.validation-graph",
    } <= cross_harness
    assert "python.repository-infrastructure.cross-skip-accounting" in _selected_tags_for_paths(
        ["cross/differential.py"]
    )


def test_python_and_rust_package_checkers_select_only_their_artifact_family() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    python_packages = {
        step.tag for step in config.steps if "scripts/check_python_packages.py" in step.cmd
    }
    rust_packages = {
        step.tag for step in config.steps if "scripts/check_rust_packages.py" in step.cmd
    }

    selected_python = _selected_tags_for_paths(["scripts/check_python_packages.py"])
    selected_rust = _selected_tags_for_paths(["scripts/check_rust_packages.py"])
    assert python_packages <= selected_python
    assert rust_packages.isdisjoint(selected_python)
    assert rust_packages <= selected_rust
    assert python_packages.isdisjoint(selected_rust)


def test_shared_pytest_infrastructure_selects_every_actual_consumer() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    component_tests = {
        step.tag
        for step in config.steps
        if "scripts/run_component_tests.py --component" in step.cmd
        or "python3 -m pytest -q tests/" in step.cmd
    }
    sharded_tests = {
        step.tag
        for step in config.steps
        if "scripts/run_pytest_shard.py" in step.cmd and "--self-test" not in step.cmd
    }
    namespace_tests = {
        step.tag for step in config.steps if "scripts/pid_namespace_init.py" in step.cmd
    }

    assert component_tests
    assert sharded_tests
    assert namespace_tests
    assert all(
        "validation-python-component-tests" in step.labels
        for step in config.steps
        if step.tag in component_tests
    )
    assert all(
        "validation-pytest-shards" in step.labels
        for step in config.steps
        if step.tag in sharded_tests
    )
    assert all(
        "validation-python-namespace-tests" in step.labels
        for step in config.steps
        if step.tag in namespace_tests
    )

    assert component_tests <= _selected_tags_for_paths(["py/tests/conftest.py"])
    assert sharded_tests <= _selected_tags_for_paths(["scripts/run_pytest_shard.py"])
    assert namespace_tests <= _selected_tags_for_paths(["scripts/pid_namespace_init.py"])


def test_rust_workspace_manifests_select_every_rust_test_without_python_tests() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    rust_tests = {step.tag for step in config.steps if step.tag.startswith("rust.")}
    unrelated_python_suites = {
        step.tag
        for step in config.steps
        if (
            step.tag.startswith("python-lifecycle.")
            or step.tag.startswith("python.")
            and not step.tag.startswith("python.repository-infrastructure.")
        )
    }
    selected = _selected_tags_for_paths(["rs/Cargo.toml"])

    assert rust_tests <= selected
    assert unrelated_python_suites.isdisjoint(selected)


def test_distinct_cli_smokes_live_in_the_canonical_graph() -> None:
    config = dag_from_path(REPO_ROOT / "validation.dag.yaml")
    by_tag = config.by_tag()

    expected_commands = {
        "python.experiment-runner.cli-smoke": "plan-round",
        "python.planner.cli-smoke": "--format actions",
        "python.tick-hub.cli-smoke": "tick-hub-state.yaml",
        "python.wrkviz.cli-smoke": "wrkviz quickstart",
        "vibe-talk.smoke.cli-help": "--help",
    }
    for tag, command_fragment in expected_commands.items():
        assert command_fragment in by_tag[tag].cmd

#!/usr/bin/env python3
"""Run the checks this change actually needs, and say what was skipped and why.

The portable, current-toolchain repository contract is expensive. A full run builds and tests all
workspace code, runs the cross-language differential over every paired tool, and then packages and
smoke-installs eight distributions and seven crates. That is the right price for a broad or
unclassified change. It is the wrong price for a change to an independent tool or to
repository-only prose: checks that cannot observe the edit only make the author wait.

So this maps changed paths onto the checks that could plausibly go red because of them and reports
the plan by name. Broad groups say RUN or skip; when component labels select only part of a broad
group, the report says narrow rather than falsely claiming that group cannot observe the edit.
A selector that quietly does less is indistinguishable from one that is broken, so expected work
is always reported explicitly rather than dropped in silence.

Two safety properties, both tested in `--self-test`:

* **An unrecognised path selects EVERYTHING.** Adding a new top-level directory must not silently
  opt out of validation. Unknown means unknown, and unknown means run the lot.
* **`--all` is always available**, and is what the nightly portable-contract path should use.
  Selection is a convenience for the edit-run loop, not a new definition of "validated".

Usage:
    python3 scripts/validate.py                 # against origin/main
    python3 scripts/validate.py --base HEAD~1
    python3 scripts/validate.py --all           # the entire portable contract, no selection
    python3 scripts/validate.py --list          # print the plan, run nothing
    python3 scripts/validate.py --self-test     # check the mapping itself, offline

HOST SETTINGS. A host may need to tell the graph which npm binary to run, a package registry
mirror, or a Node release mirror -- facts about the machine that the repository must not name.
They go in ${XDG_CONFIG_HOME:-~/.config}/agent-utils/validate.env, one KEY=VALUE per line ('#'
comments and blank lines allowed, VALUE taken literally), and only these keys are accepted:
AGENT_UTILS_NPM, AGENT_UTILS_NODE_DIST_URL, npm_config_registry. Any other key, a malformed or
repeated line, or an empty value stops validation before it starts. The values reach the
validation graph's environment and nothing else; a variable already set in the caller's
environment wins. Each run prints the file and the keys it set, never their values.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import NamedTuple

from run_component_tests import Manifest, components_for_paths, load_manifest

REPO_ROOT = Path(__file__).resolve().parent.parent


DOCS = "docs"
# Full-impact repository wiring (Makefile, setup, validation graph, workflows) activates every
# group, including this umbrella for component-only nodes. It deliberately has no narrow emitter.
WORKSPACE = "workspace"
PYTHON = "python"
PYTHON_COMPONENT_TESTS = "python-component-tests"
PYTEST_SHARDS = "pytest-shards"
PYTHON_NAMESPACE_TESTS = "python-namespace-tests"
RUST = "rust"
RUST_WORKSPACE_TESTS = "rust-workspace-tests"
CROSS = "cross"
PACKAGES = "packages"
PYTHON_PACKAGES = "python-packages"
RUST_PACKAGES = "rust-packages"
VIBE_TALK = "vibe-talk"
TIMELINE_BROWSER = "timeline-browser"
HYGIENE = "hygiene"

#: Logical selection groups map one-to-one onto labels in the flattened validation DAG. A full
#: run omits label selection and therefore executes every node exactly once.
GROUPS: tuple[str, ...] = (
    HYGIENE,
    DOCS,
    PYTHON,
    PYTHON_COMPONENT_TESTS,
    PYTEST_SHARDS,
    PYTHON_NAMESPACE_TESTS,
    RUST,
    RUST_WORKSPACE_TESTS,
    WORKSPACE,
    CROSS,
    PACKAGES,
    PYTHON_PACKAGES,
    RUST_PACKAGES,
    TIMELINE_BROWSER,
    VIBE_TALK,
)

GROUP_LABELS: dict[str, str] = {
    HYGIENE: "validation-hygiene",
    DOCS: "validation-docs",
    PYTHON: "validation-python",
    PYTHON_COMPONENT_TESTS: "validation-python-component-tests",
    PYTEST_SHARDS: "validation-pytest-shards",
    PYTHON_NAMESPACE_TESTS: "validation-python-namespace-tests",
    RUST: "validation-rust",
    RUST_WORKSPACE_TESTS: "validation-rust-workspace-tests",
    WORKSPACE: "validation-workspace",
    CROSS: "validation-cross",
    PACKAGES: "validation-packages",
    PYTHON_PACKAGES: "validation-python-packages",
    RUST_PACKAGES: "validation-rust-packages",
    TIMELINE_BROWSER: "validation-timeline",
    VIBE_TALK: "validation-vibe-talk",
}

ALL_GROUPS: frozenset[str] = frozenset(GROUPS)

#: Groups that run for EVERY change, whatever the paths. A group belongs here only if a change
#: anywhere could break it -- which for a repo-wide text scan is the literal truth.
ALWAYS: frozenset[str] = frozenset({HYGIENE})

#: Why a group runs, phrased so the reason survives being read months later.
WHY: dict[str, str] = {
    DOCS: "a document that is embedded into packaged user guides changed",
    PYTHON: "Python source or its validation configuration changed",
    PYTHON_COMPONENT_TESTS: "shared Python component-test infrastructure changed",
    PYTEST_SHARDS: "the pytest sharding implementation changed",
    PYTHON_NAMESPACE_TESTS: "the mapped-namespace test launcher changed",
    RUST: "Rust source or its validation configuration changed",
    RUST_WORKSPACE_TESTS: "shared Rust workspace manifests or locks changed",
    WORKSPACE: "shared workspace configuration changed",
    CROSS: "code with a paired cross-language implementation changed",
    PACKAGES: "something that ships inside a distribution changed",
    PYTHON_PACKAGES: "Python packaging inputs or checks changed",
    RUST_PACKAGES: "Rust packaging inputs or checks changed",
    VIBE_TALK: "vibe-talk changed (it is outside the Rust workspace and has its own suite)",
    TIMELINE_BROWSER: (
        "the timeline's browser-facing assets or its browser suite changed; these drive a real"
        " Chromium and are the only checks that can see a rendering or interaction regression"
    ),
    HYGIENE: (
        "always: a consuming project's name can be typed into any file, a documented default"
        " can be broken from either the prose or the code side, and renaming any file or"
        " rewording any heading can break a website link"
    ),
}

#: Longest prefix wins, so `scripts/embed_userguides.py` beats the bare `scripts/` catch-all.
PREFIX_RULES: tuple[tuple[str, frozenset[str]], ...] = (
    # The tool's own gate includes strict typing for its Python operator scripts. Keeping that
    # check beside the scripts means a one-line helper fix no longer drags in the unrelated Rust
    # and Python workspaces merely to reach the repository-wide mypy invocation.
    ("vibe-talk/scripts/", frozenset({VIBE_TALK})),
    ("vibe-talk/", frozenset({VIBE_TALK})),
    # Shared workspace manifests and test configuration can affect every component. Tool-owned
    # source below these paths is resolved through validation/components.json instead.
    # Workspace dependency, feature, and profile changes can alter both implementations' runtime
    # behavior even when no paired source file changed, so they owe the parity contract too.
    (
        "rs/Cargo.toml",
        frozenset({RUST, RUST_WORKSPACE_TESTS, CROSS, RUST_PACKAGES}),
    ),
    (
        "rs/Cargo.lock",
        frozenset({RUST, RUST_WORKSPACE_TESTS, CROSS, RUST_PACKAGES}),
    ),
    # Every public Rust launcher dispatches through this one script. A change can affect every
    # tool, package smoke, and cross-runtime invocation, so treating it as one component would be
    # false precision; run the complete portable contract explicitly.
    ("rs/bin/cargo-runner", frozenset(GROUPS)),
    ("rustfmt.toml", frozenset({RUST, VIBE_TALK})),
    (
        "py/pyproject.toml",
        frozenset({PYTHON, PYTHON_COMPONENT_TESTS, PYTEST_SHARDS, PYTHON_PACKAGES}),
    ),
    # These are imported by tests across component and lifecycle lanes. No single tool owns their
    # effects, so use the conservative all-workspace test boundary.
    ("py/tests/__init__.py", frozenset({PYTHON, PYTHON_COMPONENT_TESTS})),
    ("py/tests/conftest.py", frozenset({PYTHON, PYTHON_COMPONENT_TESTS})),
    # Browser assets are also owned by wrkviz through the component manifest; this group adds the
    # real browser checks to that component's unit/package/cross closure.
    ("py/wrkviz/static/", frozenset({TIMELINE_BROWSER, PYTHON})),
    ("py/tests/js/", frozenset({TIMELINE_BROWSER, PYTHON})),
    # The language indexes are package-check inputs: each checker verifies that its public index
    # names exactly the distributions it ships. These exact rules must not bleed into a future
    # `py/README.md.generated`, which `_rule_matches` prevents.
    ("py/README.md", frozenset({PYTHON_PACKAGES})),
    ("rs/README.md", frozenset({RUST_PACKAGES})),
    # gh-paced is a Rust-only tool: it has no Python twin, no cross-language pair, and ships in no
    # package. validation/components.json requires Python component tests, so it is routed here
    # instead. Its crate tests carry the Rust workspace-tests label. It embeds its guides with
    # include_str!, so a guide edit also rebuilds and tests the crate.
    ("rs/gh-paced/", frozenset({RUST, RUST_WORKSPACE_TESTS})),
    ("common/docs/gh-paced/", frozenset({DOCS, RUST, RUST_WORKSPACE_TESTS})),
    # agent-usage is Rust-only for the same reasons as gh-paced, and embeds its guides the same way.
    ("rs/agent-usage/", frozenset({RUST, RUST_WORKSPACE_TESTS})),
    ("common/docs/agent-usage/", frozenset({DOCS, RUST, RUST_WORKSPACE_TESTS})),
    ("cross/", frozenset({CROSS})),
    ("common/docs/", frozenset({DOCS})),
    ("common/README.md", frozenset()),
    ("skills/agentctl/", frozenset({DOCS})),
    ("examples/", frozenset({CROSS})),
    # The log fetcher and its shell wrapper. Type-checked by `make check` (mypy walks the repo)
    # and covered by `py/tests/test_agent_log_archive_fetcher.py`, so it owes the workspace
    # group -- and nothing else: it ships in no distribution and has no cross-language twin.
    # Before this rule it read as unclassified and selected EVERY group, which is the safe
    # direction to be wrong in but meant a one-line fetcher change ran vibe-talk's Rust suite.
    ("scripts/agent-log-archive/", frozenset({PYTHON})),
    ("scripts/embed_userguides.py", frozenset({DOCS, PACKAGES})),
    ("scripts/check_python_packages.py", frozenset({PYTHON_PACKAGES})),
    ("scripts/check_rust_packages.py", frozenset({RUST_PACKAGES})),
    ("scripts/with-node22", frozenset({TIMELINE_BROWSER})),
    ("scripts/provision-node-packages", frozenset({TIMELINE_BROWSER, VIBE_TALK})),
    ("scripts/provision-node22", frozenset({TIMELINE_BROWSER})),
    # These define the planner or executable graph itself; only the complete portable contract can prove a
    # change to them. The exact rules beat the repository-infrastructure component mapping.
    ("scripts/validate.py", frozenset(GROUPS)),
    ("scripts/run_component_tests.py", frozenset({PYTHON_COMPONENT_TESTS})),
    ("scripts/run_pytest_shard.py", frozenset({PYTEST_SHARDS})),
    ("scripts/pid_namespace_init.py", frozenset({PYTHON_NAMESPACE_TESTS})),
    ("validation.dag.yaml", frozenset(GROUPS)),
    ("validation/", frozenset(GROUPS)),
    # Their own group is always-on, so naming them here only stops them reading as unclassified.
    ("scripts/check_client_names.py", frozenset()),
    ("scripts/check_documented_defaults.py", frozenset()),
    ("scripts/check_site.py", frozenset()),
    # The public website is static HTML and CSS that nothing builds or embeds. Its checker runs in
    # the always-on hygiene group because the site links into the whole tree: a renamed document
    # or a reworded heading ANYWHERE can break it, so a site-only rule here could not be complete.
    # site/demos/ holds the Python that records the site's media clips; a Python file there owes
    # repository-wide strict typing, which the root-mypy rule in selection_for adds.
    ("site/", frozenset()),
    ("scripts/check_no_any.py", frozenset({PYTHON, CROSS, VIBE_TALK})),
    # Prose and agent-facing material. Not embedded anywhere, so nothing can go red for it.
    (".minibeads/", frozenset()),
    ("ai_docs/", frozenset()),
    ("reviews/", frozenset()),
    ("skills/", frozenset()),
    ("bin/", frozenset()),
    ("common/bin/", frozenset(GROUPS)),
    ("Makefile", frozenset(GROUPS)),
    ("setup", frozenset(GROUPS)),
    ("README.md", frozenset()),
    ("QUICKSTART.md", frozenset()),
    ("scripts/README.md", frozenset()),
    ("cross/README.md", frozenset()),
    ("AGENTS.md", frozenset()),
    ("CLAUDE.md", frozenset()),
    ("LICENSE", frozenset({PACKAGES})),
    (".gitignore", frozenset()),
    # Workflow edits can change conditional compatibility jobs or scheduled behavior that a
    # path-filtered self-run would otherwise skip. Exercise the complete portable graph and make
    # every plan output true so the edited compatibility lanes run as well.
    (".github/", frozenset(GROUPS)),
    # The one exception: the Pages workflow has no compatibility lane and no validation of its own.
    # It checks and stages site/ (minus demos/ and README.md) with scripts/check_site.py and
    # uploads the staged copy, and the hygiene group already runs that checker on every change.
    # The component manifest still selects the infrastructure tests.
    (".github/workflows/pages.yml", frozenset()),
)


def _rule_matches(path: str, prefix: str) -> bool:
    """Match a directory prefix or an exact file rule without prefix bleed.

    Directory rules end in ``/`` deliberately. Every other rule names one exact path: treating
    ``AGENTS.md`` as a string prefix also exempted paths such as ``AGENTS.md.generated`` from the
    fail-safe all-checks behavior.
    """
    return path.startswith(prefix) if prefix.endswith("/") else path == prefix


def selection_for(
    paths: list[str], *, manifest: Manifest | None = None
) -> tuple[frozenset[str], dict[str, str], frozenset[str], dict[str, str]]:
    """Map changed paths onto validation groups and tool components.

    A component match selects only that tool and its declared reverse dependencies. An
    unrecognised path still selects every group and names itself as the reason.
    """
    active_manifest = manifest if manifest is not None else load_manifest()
    components, component_because = components_for_paths(paths, active_manifest)
    selected: set[str] = set(ALWAYS)
    because: dict[str, str] = {group: "always" for group in ALWAYS}
    for path in sorted(paths):
        path_components, _ = components_for_paths([path], active_manifest)
        best: frozenset[str] | None = None
        best_len = -1
        for prefix, groups in PREFIX_RULES:
            if _rule_matches(path, prefix):
                if len(prefix) > best_len:
                    best, best_len = groups, len(prefix)
        # Root `make mypy` scans the whole checkout, not only `py/`. A Python helper stored in an
        # otherwise prose-only directory is executable validation input and must not inherit the
        # directory's exemption. That includes the website's demo recorders under site/demos/.
        # Vibe-talk's operator scripts are the one explicit exception: its own selected gate runs
        # the same strict mypy contract over that directory.
        root_mypy_observes = (
            Path(path).suffix in {".py", ".pyi"}
            and path.startswith(("ai_docs/", "reviews/", "skills/", "common/docs/", "site/"))
            and (best is None or WORKSPACE not in best)
        )
        if root_mypy_observes:
            selected.add(PYTHON)
            because.setdefault(PYTHON, path)
            if best is None:
                best = frozenset()
        if path_components:
            # Component ownership answers WHICH tool. These labels cover shared language-level
            # checks that are intentionally not duplicated in every tool fragment.
            if path.startswith("py/") or path.startswith("scripts/"):
                selected.add(PYTHON)
                because.setdefault(PYTHON, path)
            if path.startswith("rs/"):
                selected.add(RUST)
                because.setdefault(RUST, path)
            if path.startswith("common/docs/"):
                selected.add(DOCS)
                because.setdefault(DOCS, path)
        if best is None:
            if path_components:
                continue
            # Fail safe. A path nobody has classified is a path nobody has reasoned about.
            for group in sorted(ALL_GROUPS):
                because.setdefault(group, f"{path} (unclassified path — running everything)")
            selected |= set(ALL_GROUPS)
            continue
        for group in sorted(best):
            because.setdefault(group, path)
        selected |= set(best)
    return frozenset(selected), because, components, component_because


def groups_for(
    paths: list[str], *, manifest: Manifest | None = None
) -> tuple[frozenset[str], dict[str, str]]:
    """Compatibility surface for tests and callers interested only in broad groups."""

    selected, because, _components, _component_because = selection_for(
        paths, manifest=manifest
    )
    return selected, because


def changed_paths(base: str, *, repo_root: Path = REPO_ROOT) -> list[str]:
    """Paths differing from `base`, including uncommitted and untracked work."""
    out: set[str] = set()
    merge_base = subprocess.run(
        ("git", "merge-base", base, "HEAD"), cwd=repo_root, capture_output=True, text=True, check=False
    )
    ref = merge_base.stdout.strip() if merge_base.returncode == 0 else base
    for argv in (
        # Disable rename collapsing deliberately. A move can cross component boundaries, so the
        # source and destination must both participate in selection: a rename from a package into
        # prose still owes the checks which could observe the removed package file, and vice versa.
        ("git", "diff", "--no-renames", "--name-only", f"{ref}...HEAD"),
        ("git", "diff", "--no-renames", "--name-only", "HEAD"),
        ("git", "ls-files", "--others", "--exclude-standard"),
    ):
        done = subprocess.run(argv, cwd=repo_root, capture_output=True, text=True, check=False)
        if done.returncode != 0:
            raise SystemExit(f"validate: `{' '.join(argv)}` failed:\n{done.stderr.strip()}")
        out.update(line for line in done.stdout.splitlines() if line)
    return sorted(out)


#: The cross-language differential writes one JSON verdict record per tool into this directory
#: (``COVERAGE_DIR_ENV`` in ``cross/differential.py``). dagrun reports a node that passed with
#: allowed skips as a plain PASS, so these records are the only way this summary can tell a
#: PARTIAL cross node from a complete one.
CROSS_COVERAGE_ENV = "AGENT_UTILS_CROSS_COVERAGE_DIR"

#: The host settings validate.env may supply (see HOST SETTINGS in the module docstring).
HOST_ENV_KEYS = ("AGENT_UTILS_NPM", "AGENT_UTILS_NODE_DIST_URL", "npm_config_registry")


class HostEnvError(ValueError):
    """validate.env is malformed or names a setting validation does not accept."""


def host_env_path(environ: Mapping[str, str]) -> Path:
    """Where this user's host settings live: $XDG_CONFIG_HOME, else ~/.config."""
    config = environ.get("XDG_CONFIG_HOME") or str(Path(environ.get("HOME") or "~").expanduser() / ".config")
    return Path(config) / "agent-utils" / "validate.env"


def parse_host_env(text: str, source: str) -> dict[str, str]:
    """Parse validate.env, refusing rather than ignoring anything it does not understand."""
    values: dict[str, str] = {}
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        key, sep, value = line.partition("=")
        key, value = key.strip(), value.strip()
        where = f"{source}:{number}"
        if not sep or not key:
            raise HostEnvError(f"{where}: expected KEY=VALUE, got {line!r}")
        if key not in HOST_ENV_KEYS:
            raise HostEnvError(f"{where}: {key!r} is not a host setting; allowed: {', '.join(HOST_ENV_KEYS)}")
        if key in values:
            raise HostEnvError(f"{where}: {key} is set twice")
        if not value:
            raise HostEnvError(f"{where}: {key} has an empty value")
        values[key] = value
    return values


def host_env(environ: Mapping[str, str]) -> dict[str, str]:
    """The settings validate.env adds to the graph's environment, announced by key only."""
    path = host_env_path(environ)
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}
    values = parse_host_env(text, str(path))
    added = {key: value for key, value in values.items() if key not in environ}
    kept = sorted(set(values) - set(added))
    line = f"validate: host settings {path} set {', '.join(sorted(added)) or 'nothing'}"
    if kept:
        line += f"; already in the environment, kept: {', '.join(kept)}"
    print(line, flush=True)
    return added


class CrossCoverage(NamedTuple):
    """What the cross verdict records of one validation run show."""

    #: Lines to print, one per record that skipped checks, record that could not be read, or
    #: selected cross node that left no readable record.
    lines: list[str]
    #: Skipped checks across all readable records.
    skipped: int
    #: Records that could not be read or did not have the expected shape.
    unreadable: int
    #: Selected cross nodes, in the order given, with no readable record naming them.
    missing: list[str]
    #: Skipped checks across all readable records, counted per skip kind and sorted by kind.
    by_kind: tuple[tuple[str, int], ...] = ()


def _kind_counts(counts: Mapping[str, int]) -> str:
    """``kind N`` for each skip kind, sorted by kind: ``boxing 3, delegated-live-scope 2``."""

    return ", ".join(f"{kind} {counts[kind]}" for kind in sorted(counts))


def cross_coverage_report(directory: Path, expected_nodes: Sequence[str]) -> CrossCoverage:
    """Describe the cross verdict records in `directory` against the nodes that should write them.

    Each node in `expected_nodes` runs ``cross/differential.py`` and must leave a record naming it.
    A record that cannot be read, and a node with no readable record, are counted rather than
    ignored: either may have been the one PARTIAL node, so the summary must not claim full
    coverage without it. Among the readable records, those that skipped checks are listed,
    including a record from a node that was not expected.
    """

    lines: list[str] = []
    skipped_total = 0
    unreadable = 0
    recorded_nodes: set[str] = set()
    by_kind: dict[str, int] = {}
    for path in sorted(directory.glob("*.json")):
        try:
            value: object = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as exc:
            lines.append(f"  unreadable cross coverage record {path.name}: {exc}")
            unreadable += 1
            continue
        skipped = value.get("skipped") if isinstance(value, dict) else None
        if not isinstance(value, dict) or not isinstance(skipped, list):
            lines.append(f"  malformed cross coverage record {path.name}: {value!r}")
            unreadable += 1
            continue
        node = value.get("node")
        if isinstance(node, str):
            recorded_nodes.add(node)
        if not skipped:
            continue
        labels: list[str] = []
        kinds: dict[str, int] = {}
        for item in skipped:
            if isinstance(item, dict):
                labels.append(str(item.get("label")))
                kind = str(item.get("kind"))
            else:
                labels.append(repr(item))
                kind = "malformed"
            kinds[kind] = kinds.get(kind, 0) + 1
            by_kind[kind] = by_kind.get(kind, 0) + 1
        skipped_total += len(skipped)
        where = value.get("node") or "top level"
        lines.append(
            f"  {where} ({value.get('tool')}, {value.get('verdict')}): {value.get('checks')} "
            f"check(s) ran; {len(skipped)} skipped [{_kind_counts(kinds)}]: "
            f"{', '.join(labels)}"
        )
    missing = [node for node in expected_nodes if node not in recorded_nodes]
    for node in missing:
        lines.append(
            f"  {node}: no readable coverage record, so its coverage could not be confirmed"
        )
    return CrossCoverage(
        lines, skipped_total, unreadable, missing, tuple(sorted(by_kind.items()))
    )


def _workflow_command_data(text: str) -> str:
    """Escape `text` as the message of a GitHub Actions workflow command."""

    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def announce_partial(verdict: str, lines: Sequence[str], environ: Mapping[str, str]) -> None:
    """Surface a PARTIAL verdict where a hosted job would otherwise show only a green tick.

    A PARTIAL run exits 0, so GitHub Actions marks its step as passed. Under Actions this emits a
    warning annotation and appends the verdict and its per-node lines to the job summary. Outside
    Actions it does nothing. A summary that cannot be written is reported, not fatal: the caller
    still prints the verdict and exits 0.
    """

    detail = "\n".join([verdict, *lines])
    if environ.get("GITHUB_ACTIONS") == "true":
        print(f"::warning title=validate PARTIAL::{_workflow_command_data(detail)}", flush=True)
    summary_path = environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    body = "\n".join(lines)
    try:
        with open(summary_path, "a", encoding="utf-8") as handle:
            handle.write(f"### validate: PARTIAL\n\n{verdict}\n\n```text\n{body}\n```\n")
    except OSError as exc:
        print(f"validate: cannot append the PARTIAL verdict to GITHUB_STEP_SUMMARY: {exc}",
              file=sys.stderr)


def run(
    selected: frozenset[str],
    components: frozenset[str],
    *,
    all_contract: bool,
    coverage_dir: Path | None = None,
    host: Mapping[str, str] | None = None,
) -> int:
    """Execute one flattened dagrun graph for the selected contract.

    With `coverage_dir`, every cross differential node writes its verdict record there. `host`
    is the validate.env settings, added to the graph's environment only.
    """

    cpu_count = os.cpu_count() or 1
    max_cpus = os.environ.get("VALIDATE_MAX_CPUS", str(min(32, cpu_count)))
    max_steps = os.environ.get("VALIDATE_MAX_STEPS", str(min(24, cpu_count)))
    command = [
        str(REPO_ROOT / "common" / "bin" / "dagrun"),
        "run",
        "--dag",
        str(REPO_ROOT / "validation.dag.yaml"),
        "--max-steps",
        max_steps,
        "--max-cpus",
        max_cpus,
        "--planner",
        "critical-path",
        "--profile",
    ]
    if not all_contract:
        labels = {GROUP_LABELS[group] for group in selected}
        labels.update(f"component-{name}" for name in components)
        command.extend(("--labels", ",".join(sorted(labels))))
    extra = os.environ.get("VALIDATE_DAGRUN_FLAGS", os.environ.get("BOX_FLAGS", ""))
    command.extend(shlex.split(extra))
    print(f"\n=== flattened validation DAG ({len(command)} argv entries) ===", flush=True)
    env = dict(os.environ)
    env.update(host or {})
    if coverage_dir is not None:
        env[CROSS_COVERAGE_ENV] = str(coverage_dir)
    done = subprocess.run(command, cwd=REPO_ROOT, check=False, env=env)
    if done.returncode != 0:
        print("\nvalidate: FAILED in flattened validation DAG", file=sys.stderr)
    return done.returncode


def report(
    selected: frozenset[str],
    because: dict[str, str],
    components: frozenset[str],
    component_because: dict[str, str],
    paths: list[str],
) -> None:
    print(f"validate: {len(paths)} changed path(s)")
    for group in sorted(ALL_GROUPS):
        if group in selected:
            print(f"  RUN   {group:<11} — {WHY[group]} [{because.get(group, '?')}]")
        elif components:
            print(
                f"  narrow {group:<11} — no all-component sweep; matching component nodes "
                "may still run below"
            )
        else:
            print(f"  skip  {group:<11} — nothing changed that it can observe")
    for component in sorted(components):
        print(
            f"  RUN   component:{component:<20} — owned tool and reverse-dependency checks "
            f"[{component_because.get(component, '?')}]"
        )
    if selected <= ALWAYS and not components:
        print("  (nothing selected by path: every changed path is prose or configuration)")


def selected_graph(
    selected: frozenset[str], components: frozenset[str], *, all_contract: bool
) -> tuple[dict[str, dict[str, object]], set[str]]:
    """Load the canonical graph with the public loader and select what `run` would run.

    Returns every step by tag, in graph order, and the tags selected by the same labels `run`
    passes to dagrun, closed over dependencies.
    """

    completed = subprocess.run(
        (
            str(REPO_ROOT / "common" / "bin" / "dagrun"),
            "json",
            "--dag",
            str(REPO_ROOT / "validation.dag.yaml"),
        ),
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise SystemExit(f"validate: cannot load canonical DAG:\n{completed.stderr.strip()}")
    raw_value: object = json.loads(completed.stdout)
    if not isinstance(raw_value, dict) or not isinstance(raw_value.get("steps"), list):
        raise SystemExit("validate: canonical DAG JSON has no steps list")
    steps: dict[str, dict[str, object]] = {}
    for index, value in enumerate(raw_value["steps"]):
        if not isinstance(value, dict):
            raise SystemExit(f"validate: canonical DAG step {index} is not an object")
        record: dict[str, object] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                raise SystemExit(f"validate: canonical DAG step {index} has a non-string key")
            record[key] = item
        group, job = record.get("group"), record.get("job")
        if not isinstance(group, str) or not isinstance(job, str):
            raise SystemExit(f"validate: canonical DAG step {index} has no string group/job")
        steps[f"{group}.{job}"] = record

    chosen = set(steps)
    if not all_contract:
        labels = {GROUP_LABELS[group] for group in selected} | {
            f"component-{component}" for component in components
        }
        chosen = set()
        for tag, step in steps.items():
            raw_labels = step.get("labels")
            if not isinstance(raw_labels, list) or not all(
                isinstance(label, str) for label in raw_labels
            ):
                raise SystemExit(f"validate: canonical DAG step {tag} has invalid labels")
            if labels.intersection(label for label in raw_labels if isinstance(label, str)):
                chosen.add(tag)
        pending = list(chosen)
        while pending:
            tag = pending.pop()
            raw_deps = steps[tag].get("deps")
            if not isinstance(raw_deps, list) or not all(
                isinstance(dep, str) for dep in raw_deps
            ):
                raise SystemExit(f"validate: canonical DAG step {tag} has invalid dependencies")
            for dependency in (dep for dep in raw_deps if isinstance(dep, str)):
                if dependency not in steps:
                    raise SystemExit(
                        f"validate: canonical DAG step {tag} names missing dependency {dependency}"
                    )
                if dependency not in chosen:
                    chosen.add(dependency)
                    pending.append(dependency)
    return steps, chosen


def selected_cross_nodes(
    selected: frozenset[str], components: frozenset[str], *, all_contract: bool
) -> list[str]:
    """Tags of the selected nodes that run ``cross/differential.py``, in graph order.

    Each of them writes a coverage record when it reaches a verdict, so after a passing run a
    missing record means that node's coverage cannot be confirmed.
    """

    steps, chosen = selected_graph(selected, components, all_contract=all_contract)
    return [
        tag
        for tag, step in steps.items()
        if tag in chosen
        and isinstance(command := step.get("cmd"), str)
        and "cross/differential.py" in command
    ]


def report_selected_graph(
    selected: frozenset[str], components: frozenset[str], *, all_contract: bool
) -> None:
    """Print the exact flattened nodes and resources selected by the public graph loader.

    CI consumes the ``NEED resource:*`` lines when provisioning optional runtimes. Deriving them
    from the selected graph prevents a component label from activating (for example) a browser
    gate while a second, coarser group-name heuristic incorrectly skips browser installation.
    """

    steps, chosen = selected_graph(selected, components, all_contract=all_contract)
    resources = sorted(
        {
            resource
            for tag in chosen
            for resource, amount in _step_resources(steps[tag]).items()
            if amount > 0
        }
    )
    for resource in resources:
        print(f"  NEED  resource:{resource}")
    for tag in steps:
        if tag in chosen:
            print(f"  STEP  {tag}")


def _step_resources(step: dict[str, object]) -> dict[str, int]:
    """Return one canonical JSON step's checked resource demand."""

    hint = step.get("hint")
    if not isinstance(hint, dict):
        raise SystemExit("validate: canonical DAG step has no hint object")
    raw_resources = hint.get("resources")
    if not isinstance(raw_resources, dict):
        raise SystemExit("validate: canonical DAG step has no resources object")
    resources: dict[str, int] = {}
    for key, value in raw_resources.items():
        if not isinstance(key, str) or not isinstance(value, int) or isinstance(value, bool):
            raise SystemExit("validate: canonical DAG step has an invalid resource demand")
        resources[key] = value
    return resources


def self_test() -> int:
    """Check mapping and rename semantics offline, without running the build."""
    failures: list[str] = []
    manifest = load_manifest()

    def expect(
        paths: list[str], want: set[str], why: str, want_components: set[str] | None = None
    ) -> None:
        """`want` is what the PATHS select. Always-on groups are added here and checked below."""
        got, _, components, _ = selection_for(paths, manifest=manifest)
        expected = want | ALWAYS
        if set(got) != expected:
            failures.append(
                f"{why}\n    paths={paths}\n    want={sorted(expected)}\n    got ={sorted(got)}"
            )
        expected_components = want_components or set()
        if set(components) != expected_components:
            failures.append(
                f"{why} (components)\n    paths={paths}\n"
                f"    want={sorted(expected_components)}\n    got ={sorted(components)}"
            )

    expect(["vibe-talk/src/ops.rs"], {VIBE_TALK}, "a vibe-talk change must not drag in the workspace contract")
    expect(["vibe-talk/web/voice.js", "vibe-talk/README.md"], {VIBE_TALK}, "vibe-talk docs are still vibe-talk")
    expect(
        ["vibe-talk/scripts/smoke-agent.py"],
        {VIBE_TALK},
        "vibe-talk's own gate type-checks its Python without selecting unrelated workspaces",
    )
    expect(
        ["rs/tick-hub/src/lib.rs"],
        {RUST},
        "a Rust tool change needs its static gate and component closure",
        {"tick-hub"},
    )
    expect(
        ["py/dagrun/sizing.py"],
        {PYTHON},
        "a Python tool change needs its static gate and reverse dependencies",
        {"dagrun", "experiment-runner"},
    )
    expect(
        ["cross/differential.py"],
        {CROSS},
        "the differential harness also selects its direct environment-regression tests",
        {"repository-infrastructure"},
    )
    expect(
        ["common/docs/herdr-run/README.template.md"],
        {DOCS},
        "embedded docs select their owning component",
        {"herdr-run"},
    )
    expect(
        ["rs/gh-paced/src/wrapper.rs"],
        {RUST, RUST_WORKSPACE_TESTS},
        "the Rust-only gh-paced tool selects the Rust gate and the crate tests, not everything",
    )
    expect(
        ["common/docs/gh-paced/USER_GUIDE.md"],
        {DOCS, RUST, RUST_WORKSPACE_TESTS},
        "gh-paced compiles its guides into the binary, so a guide edit re-tests the crate",
    )
    expect(
        ["rs/agent-usage/src/burn.rs"],
        {RUST, RUST_WORKSPACE_TESTS},
        "the Rust-only agent-usage tool selects the Rust gate and the crate tests, not everything",
    )
    expect(
        ["common/docs/agent-usage/USER_GUIDE.md"],
        {DOCS, RUST, RUST_WORKSPACE_TESTS},
        "agent-usage compiles its guides into the binary, so a guide edit re-tests the crate",
    )
    expect(
        ["skills/agentctl/SKILL.md"],
        {DOCS},
        "the bundled agentctl skill selects its owning component",
        {"agentctl"},
    )
    expect(
        ["skills/herdr-run/SKILL.md"],
        set(),
        "the bundled herdr-run skill selects the tests that read it",
        {"herdr-run"},
    )
    expect(
        ["skills/parallel-experiment-runner/SKILL.md"],
        set(),
        "the bundled experiment-runner skill selects the tests that read it",
        {"experiment-runner"},
    )
    expect(
        ["skills/dagrun/SKILL.md", "skills/cpuset-alloc/SKILL.md"],
        set(),
        "bundled dagrun-family skills select their owning component",
        {"dagrun", "experiment-runner"},
    )
    expect(
        ["skills/pr-landing-planner/SKILL.md", "skills/pr-landing-operations/SKILL.md"],
        set(),
        "bundled landing skills select their owning component",
        {"planner"},
    )
    expect(
        ["skills/tick-hub/SKILL.md", "skills/wrkslots/SKILL.md", "skills/wrkviz/SKILL.md"],
        set(),
        "remaining bundled tool skills select their owning components",
        {"tick-hub", "wrkslots", "wrkviz"},
    )
    expect(["AGENTS.md"], set(), "the agent guide is embedded nowhere")
    expect(["ai_docs/note.md", "reviews/x.md"], set(), "prose selects nothing")
    expect(
        ["ai_docs/transient/audit.py", "reviews/check.py"],
        {PYTHON},
        "Python audit helpers are inputs to repository-wide strict typing",
    )
    expect(
        ["skills/new-skill/helper.py"],
        {PYTHON},
        "Python code cannot inherit a surrounding prose-only exemption",
    )
    expect(
        ["reviews/types.pyi"],
        {PYTHON},
        "Python typing stubs cannot inherit a surrounding prose-only exemption",
    )
    expect(
        ["common/docs/example.py"],
        {DOCS, PYTHON},
        "a Python file under embedded docs owes both generation and strict typing",
    )
    expect([".minibeads/issues/agent-utils-1.md"], set(), "issue metadata is not executable input")
    expect(
        ["README.md", "common/README.md", "scripts/README.md"],
        set(),
        "repository overview documents are not packaged inputs",
    )
    expect(
        ["py/README.md"],
        {PYTHON_PACKAGES},
        "the Python package index selects only Python artifact checks",
    )
    expect(
        ["rs/README.md"],
        {RUST_PACKAGES},
        "the Rust package index selects only Rust artifact checks",
    )
    expect(
        ["LICENSE"],
        {PACKAGES},
        "the repository license is copied into every published artifact",
    )
    expect(
        ["rs/Cargo.toml", "rs/Cargo.lock"],
        {RUST, RUST_WORKSPACE_TESTS, CROSS, RUST_PACKAGES},
        "Rust workspace manifests and locks affect builds, runtime parity, and package archives",
    )
    expect(
        ["rs/bin/cargo-runner"],
        set(ALL_GROUPS),
        "the shared Rust launcher affects every tool and must select the complete portable contract",
        {"repository-infrastructure"},
    )
    expect(
        ["rustfmt.toml"],
        {RUST, VIBE_TALK},
        "the repository formatting policy selects both Rust workspaces",
    )
    expect(
        ["py/tests/__init__.py", "py/tests/conftest.py"],
        {PYTHON, PYTHON_COMPONENT_TESTS},
        "shared pytest infrastructure selects typing and Python component tests without Rust",
    )
    expect(
        ["py/pyproject.toml"],
        {PYTHON, PYTHON_COMPONENT_TESTS, PYTEST_SHARDS, PYTHON_PACKAGES},
        "the Python workspace manifest affects Python checks, tests, and package archives",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/run_component_tests.py"],
        {PYTHON, PYTHON_COMPONENT_TESTS},
        "the component runner selects every component test without Rust",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/run_pytest_shard.py"],
        {PYTHON, PYTEST_SHARDS},
        "the sharder selects exactly its component and lifecycle consumers",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/pid_namespace_init.py"],
        {PYTHON, PYTHON_NAMESPACE_TESTS},
        "the namespace launcher selects only mapped-namespace lifecycle consumers",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/check_python_packages.py"],
        {PYTHON, PYTHON_PACKAGES},
        "the Python package checker selects no Rust package gates",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/check_rust_packages.py"],
        {PYTHON, RUST_PACKAGES},
        "the Rust package checker selects no Python package gates",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/embed_userguides.py"],
        {DOCS, PACKAGES, PYTHON},
        "the docs generator selects docs, artifacts, typing, and its owned tests",
        {"repository-infrastructure"},
    )
    expect(
        ["py/tests/test_tickhub_cli.py"],
        {PYTHON},
        "a test edit selects its owning component rather than every Python tool",
        {"tick-hub"},
    )
    expect(["scripts/something_new.py"], set(ALL_GROUPS), "an unclassified script must select EVERYTHING")
    expect(["brand_new_toplevel/x"], set(ALL_GROUPS), "an unclassified top level must select EVERYTHING")
    expect(["AGENTS.md.generated"], set(ALL_GROUPS), "an exact-file exemption must not bleed into a prefix")
    expect(
        ["scripts/embed_userguides.py.backup"],
        set(ALL_GROUPS),
        "an exact script rule must not exempt a similarly named file",
    )
    expect(["ai_docs_extra/note.md"], set(ALL_GROUPS), "a directory rule must stop at its boundary")
    expect(
        ["scripts/with-node22"],
        {TIMELINE_BROWSER},
        "the pinned Node launcher is an input to the browser contract",
    )
    expect(
        ["scripts/provision-node-packages"],
        {TIMELINE_BROWSER, VIBE_TALK},
        "the npm provisioning step is an input to both suites that install packages",
    )
    expect(
        ["scripts/provision-node22"],
        {TIMELINE_BROWSER},
        "the pinned Node 22 release is an input to the browser contract",
    )
    expect(
        [".github/workflows/cross-dagrun.yml"],
        set(ALL_GROUPS),
        "a workflow edit runs the complete graph and every conditional compatibility lane",
        {"repository-infrastructure"},
    )
    expect(
        [".github/workflows/pages.yml"],
        set(),
        "the Pages deploy workflow runs only the website checker, which hygiene always runs",
        {"repository-infrastructure"},
    )
    expect(
        [".github/workflows/pages.yml.disabled"],
        set(ALL_GROUPS),
        "the Pages exemption names one exact file and must not bleed into other workflows",
        {"repository-infrastructure"},
    )
    expect(
        ["site/index.html", "site/style.css", "site/.nojekyll"],
        set(),
        "website files are checked by the always-on hygiene group and owe nothing else",
    )
    expect(["site.html"], set(ALL_GROUPS), "the site/ directory rule must stop at its boundary")
    expect(
        ["site/demos/vibe-talk/record_demo.py", "site/demos/vibe-talk/gchat_bridge_mock.py"],
        {PYTHON},
        "the website's demo recorders are inputs to repository-wide strict typing",
    )
    expect(
        [
            "site/demos/vibe-talk/README.md",
            "site/demos/vibe-talk/vibe-talk.toml.template",
            "site/demos/vibe-talk/fonts.conf",
            "site/media/vibe-talk/live-message.gif",
        ],
        set(),
        "demo prose, configuration and published clips are checked by the always-on hygiene group",
    )
    expect(
        ["py/tests/js/browser/new-benchmark.test.cjs"],
        {TIMELINE_BROWSER, PYTHON},
        "a new browser test selects both its runtime and the graph inventory guard",
        {"repository-infrastructure", "wrkviz"},
    )
    expect(
        ["vibe-talk/tests/js/new-page.test.mjs"],
        {VIBE_TALK},
        "a new vibe page suite selects the service and the graph inventory guard",
        {"repository-infrastructure"},
    )
    expect(
        ["vibe-talk/src/ops.rs", "rs/tick-hub/src/lib.rs"],
        {VIBE_TALK, RUST},
        "a change spanning two areas is the UNION, never the smaller of the two",
        {"tick-hub"},
    )
    expect(
        ["scripts/check_client_names.py"],
        {PYTHON},
        "a hygiene implementation change also runs its owned infrastructure tests",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/check_documented_defaults.py"],
        {PYTHON},
        "a documented-default implementation change also runs its owned tests",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/check_site.py"],
        {PYTHON},
        "a website-checker change runs strict typing and the infrastructure tests",
        {"repository-infrastructure"},
    )
    expect(
        ["scripts/check_no_any.py"],
        {PYTHON, CROSS, VIBE_TALK},
        "the no-Any checker selects every validation mode that invokes it",
        {"repository-infrastructure"},
    )
    expect([], set(), "no changes selects nothing by path")

    # ALWAYS is the property the table above cannot state, because it folds it in silently.
    for paths, why in (
        ([], "no changes at all"),
        (["ai_docs/note.md"], "a prose path that selects nothing else"),
        (["AGENTS.md"], "a file mapped to the empty set"),
        (["rs/tick-hub/src/lib.rs"], "an ordinary source change"),
    ):
        got, because = groups_for(paths, manifest=manifest)
        if not ALWAYS <= set(got):
            failures.append(f"an always-on group must run for {why}: got {sorted(got)}")
        for group in ALWAYS:
            if group not in because:
                failures.append(f"an always-on group must state a reason for {why}")

    # Every group named by a rule must exist, or the selector silently runs nothing for it.
    for prefix, groups in PREFIX_RULES:
        for group in groups:
            if group not in GROUPS:
                failures.append(f"rule {prefix!r} names unknown group {group!r}")
    for group in GROUPS:
        if group not in WHY:
            failures.append(f"group {group!r} has no stated reason in WHY")

    # Unknown paths run everything at execution time, but repository files should not remain
    # permanently unknown: that would turn the fail-safe fallback into an undocumented routing
    # policy and make selective-validation claims impossible to audit.
    inventory = subprocess.run(
        ("git", "ls-files", "--cached", "--others", "--exclude-standard"),
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if inventory.returncode != 0:
        failures.append(f"cannot inventory repository paths: {inventory.stderr.strip()}")
    else:
        unclassified: list[str] = []
        for path in sorted(line for line in inventory.stdout.splitlines() if line):
            _selected, path_because, _components, _component_because = selection_for(
                [path], manifest=manifest
            )
            if any("unclassified path" in reason for reason in path_because.values()):
                unclassified.append(path)
        if unclassified:
            failures.append(
                "every repository path must have explicit validation ownership; "
                f"unclassified={unclassified}"
            )

    # A rename is both a deletion from the old validation boundary and an addition to the new
    # one. Git's default --name-only output collapses it to the destination and can thereby skip
    # the source's tests, so exercise the real Git behavior rather than merely inspecting argv.
    with tempfile.TemporaryDirectory(prefix="agent-utils-validate-rename-") as raw:
        repository = Path(raw)
        (repository / "old-component").mkdir()
        old_path = repository / "old-component" / "source.py"
        old_path.write_text("before\n", encoding="utf-8")
        setup_commands = (
            ("git", "init", "-q"),
            ("git", "add", "old-component/source.py"),
            (
                "git",
                "-c",
                "user.name=validation self-test",
                "-c",
                "user.email=validation-self-test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "fixture",
            ),
        )
        for command in setup_commands:
            done = subprocess.run(command, cwd=repository, capture_output=True, text=True, check=False)
            if done.returncode != 0:
                failures.append(
                    f"rename-safety fixture setup failed: {' '.join(command)}: {done.stderr.strip()}"
                )
                break
        else:
            (repository / "new-component").mkdir()
            moved = subprocess.run(
                ("git", "mv", "old-component/source.py", "new-component/source.py"),
                cwd=repository,
                capture_output=True,
                text=True,
                check=False,
            )
            if moved.returncode != 0:
                failures.append(f"rename-safety fixture move failed: {moved.stderr.strip()}")
            else:
                committed = subprocess.run(
                    (
                        "git",
                        "-c",
                        "user.name=validation self-test",
                        "-c",
                        "user.email=validation-self-test@example.invalid",
                        "-c",
                        "commit.gpgsign=false",
                        "-c",
                        "core.hooksPath=/dev/null",
                        "commit",
                        "-qm",
                        "rename fixture",
                    ),
                    cwd=repository,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if committed.returncode != 0:
                    failures.append(
                        f"rename-safety fixture commit failed: {committed.stderr.strip()}"
                    )
                else:
                    expected = ["new-component/source.py", "old-component/source.py"]
                    committed_paths = changed_paths("HEAD~1", repo_root=repository)
                    if committed_paths != expected:
                        failures.append(
                            "changed_paths must retain both sides of a committed rename\n"
                            f"    want={expected}\n    got ={committed_paths}"
                        )

    good = parse_host_env(
        "# host\n\nAGENT_UTILS_NPM = /usr/bin/npm\nnpm_config_registry=https://r.example/\n", "t"
    )
    if good != {"AGENT_UTILS_NPM": "/usr/bin/npm", "npm_config_registry": "https://r.example/"}:
        failures.append(f"validate.env: a valid file parsed as {good!r}")
    for bad, why in (
        ("PATH=/tmp/evil\n", "an unknown key"),
        ("AGENT_UTILS_NPM\n", "a line without '='"),
        ("AGENT_UTILS_NPM=a\nAGENT_UTILS_NPM=b\n", "a repeated key"),
        ("npm_config_registry=\n", "an empty value"),
    ):
        try:
            parse_host_env(bad, "t")
        except HostEnvError:
            continue
        failures.append(f"validate.env: {why} was accepted instead of refused")
    if host_env_path({"XDG_CONFIG_HOME": "/x", "HOME": "/h"}) != Path("/x/agent-utils/validate.env"):
        failures.append("validate.env: XDG_CONFIG_HOME was not honoured")
    if host_env_path({"XDG_CONFIG_HOME": "", "HOME": "/h"}) != Path("/h/.config/agent-utils/validate.env"):
        failures.append("validate.env: an empty XDG_CONFIG_HOME did not fall back to ~/.config")
    with tempfile.TemporaryDirectory(prefix="validate-host-env-") as config:
        if host_env({"XDG_CONFIG_HOME": config}) != {}:
            failures.append("validate.env: a missing file added settings")
        (Path(config) / "agent-utils").mkdir()
        (Path(config) / "agent-utils" / "validate.env").write_text(
            "AGENT_UTILS_NPM=/opt/npm\nnpm_config_registry=https://r.example/\n", encoding="utf-8"
        )
        added = host_env({"XDG_CONFIG_HOME": config, "AGENT_UTILS_NPM": "/caller/npm"})
        if added != {"npm_config_registry": "https://r.example/"}:
            failures.append(f"validate.env: the caller's own setting did not win: {added!r}")

    for failure in failures:
        print(f"FAIL  {failure}", file=sys.stderr)
    if failures:
        print(f"\n{len(failures)} self-test failure(s)", file=sys.stderr)
        return 1
    print("validate --self-test: PASSED")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="origin/main", help="compare against this ref (default: origin/main)")
    parser.add_argument(
        "--all",
        action="store_true",
        help="run the entire portable, current-toolchain contract, selecting nothing",
    )
    parser.add_argument("--list", action="store_true", help="print the plan and exit without running")
    parser.add_argument("--self-test", action="store_true", help="check the mapping offline, then exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    selected: frozenset[str]
    because: dict[str, str]
    components: frozenset[str]
    component_because: dict[str, str]
    paths: list[str]
    if args.all:
        selected, because, components, component_because, paths = (
            ALL_GROUPS,
            {g: "--all" for g in ALL_GROUPS},
            frozenset(),
            {},
            [],
        )
        print("validate: --all, running the entire portable, current-toolchain contract")
    else:
        paths = changed_paths(args.base)
        selected, because, components, component_because = selection_for(paths)
        report(selected, because, components, component_because, paths)

    if args.list:
        report_selected_graph(
            selected,
            components,
            all_contract=args.all or selected == ALL_GROUPS,
        )
        return 0

    try:
        host = host_env(os.environ)
    except HostEnvError as error:
        print(f"validate: {error}", file=sys.stderr)
        return 2

    all_contract = args.all or selected == ALL_GROUPS
    # Every selected cross node must leave a record; one that does not cannot be confirmed.
    expected = selected_cross_nodes(selected, components, all_contract=all_contract)
    with tempfile.TemporaryDirectory(prefix="validate-cross-coverage-") as coverage:
        code = run(selected, components, all_contract=all_contract, coverage_dir=Path(coverage), host=host)
        if code != 0:
            return code
        coverage_state = cross_coverage_report(Path(coverage), expected)
    if coverage_state.skipped or coverage_state.unreadable or coverage_state.missing:
        # Every selected node passed, but a cross node that declared itself partial passes with
        # checks that never ran. Say so here, where "OK" would otherwise be the last word.
        unconfirmed = coverage_state.unreadable or coverage_state.missing
        state = "PARTIAL or could not be confirmed" if unconfirmed else "PARTIAL"
        print(f"\nvalidate: cross-language coverage was {state}:")
        for line in coverage_state.lines:
            print(line)
        reasons: list[str] = []
        if coverage_state.skipped:
            reasons.append(
                f"{coverage_state.skipped} cross check(s) were skipped and are UNVERIFIED "
                f"({_kind_counts(dict(coverage_state.by_kind))})"
            )
        if coverage_state.unreadable:
            reasons.append(f"{coverage_state.unreadable} coverage record(s) could not be read")
        if coverage_state.missing:
            reasons.append(
                f"{len(coverage_state.missing)} selected cross node(s) left no readable "
                "coverage record"
            )
        verdict = f"validate: PARTIAL - every selected node passed, but {'; '.join(reasons)}"
        announce_partial(verdict, coverage_state.lines, os.environ)
        print(f"\n{verdict}")
        return 0
    print("\nvalidate: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

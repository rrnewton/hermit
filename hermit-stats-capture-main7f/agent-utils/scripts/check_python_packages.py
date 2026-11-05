#!/usr/bin/env python3
"""Build, inspect, and smoke independently published Python tools.

The check is intentionally artifact-first. Each project is copied out of the
working tree, built as both a wheel and source distribution with package-index
access disabled, rebuilt from that source archive, installed alone into a fresh
virtual environment without dependencies, and started with socket access
blocked. This catches source-tree success that masks missing modules, leaked
sibling packages, import-time networking, or omitted resources.

The build environment must already provide the PEP 517 backend named by each
project. No backend or runtime dependency is downloaded by this script.

With no selector, every project is checked. ``--project`` may be repeated to
check only named distributions (or their source-directory aliases) while the
global project registry and package index remain checked.
"""

from __future__ import annotations

import argparse
import ast
import configparser
import email.parser
import email.policy
import importlib
import importlib.metadata
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tarfile
import venv
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

REPO_ROOT = Path(__file__).resolve().parent.parent
PY_ROOT = REPO_ROOT / "py"


@dataclass(frozen=True)
class Project:
    directory: str
    distribution: str
    package: str
    commands: tuple[str, ...]
    resources: tuple[str, ...]
    required_dependencies: tuple[str, ...]
    command_userguides: tuple[tuple[str, str], ...] = ()
    #: Doc-lint terms that do not apply to this distribution, because the term is the tool's
    #: SUBJECT MATTER rather than an accidental reference to a sibling implementation. Named per
    #: project so a waiver stays visible instead of becoming a hole in the rule.
    doc_term_exemptions: tuple[str, ...] = ()
    #: Sibling distributions this package deliberately composes with and therefore must name.
    sibling_package_exemptions: tuple[str, ...] = ()
    #: CLI-first documentation may live directly in its owning package.
    package_owned_docs: bool = False


PROJECTS: tuple[Project, ...] = (
    Project(
        directory="dagrun",
        distribution="dagrun",
        package="dagrun",
        commands=("dagrun", "cpuset-alloc"),
        resources=("README.md", "USER_GUIDE.md", "py.typed"),
        required_dependencies=("pyyaml",),
    ),
    Project(
        directory="tick_hub",
        distribution="tick-hub",
        package="tick_hub",
        commands=("tick-hub",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "py.typed",
            "examples/tick-hub-ops.json",
            "examples/tick-hub-ops.yaml",
            "examples/tick-hub-state.yaml",
        ),
        required_dependencies=("pyyaml",),
    ),
    Project(
        directory="pr_landing_planner",
        distribution="pr-landing-planner",
        package="pr_landing_planner",
        commands=("pr-landing-planner",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "py.typed",
            "examples/flaky-signatures.yaml",
            "examples/pr-landing-demo.yaml",
            "check_outcome.py",
        ),
        required_dependencies=("pyyaml",),
    ),
    Project(
        directory="wrkviz",
        distribution="wrkviz",
        package="wrkviz",
        commands=("wrkviz",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "py.typed",
            "static/index.html",
            "static/app.js",
            "static/timeline-core.js",
            "static/style.css",
            "static/vendor/README.md",
            "static/vendor/markdown-it-15.0.0.min.js",
            "static/vendor/markdown-it-LICENSE.txt",
        ),
        required_dependencies=(),
    ),
    Project(
        directory="parallel_experiment_runner",
        distribution="parallel-experiment-runner",
        package="parallel_experiment_runner",
        commands=("parallel-experiment-runner",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "py.typed",
            "examples/chaos-sweep.json",
        ),
        required_dependencies=("dagrun",),
        sibling_package_exemptions=("dagrun",),
    ),
    Project(
        directory="herdr_run",
        distribution="herdr-run",
        package="herdr_run",
        commands=("herdr-run",),
        # The distribution allowlists `cargo` as a target program, so its docs must name it. This is
        # user-visible subject matter, not a reference to the sibling implementation.
        doc_term_exemptions=("cargo",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "QUICKSTART.md",
            # There is deliberately no `examples/` here. `config_template.yaml` is what
            # `herdr-run init` writes, and it is the configuration reference; a second, partial
            # example file in the wheel would be a duplicate free to drift away from it.
            "config_template.yaml",
            "py.typed",
        ),
        # The configuration file is read by the package's own YAML-subset parser.
        required_dependencies=(),
    ),
    Project(
        directory="agentctl", distribution="agentctl", package="agentctl",
        commands=("agentctl", "herdr-agent", "herdr-subagents", "herdr-chat"),
        resources=("README.md", "USER_GUIDE.md", "QUICKSTART.md", "py.typed",
                   "AGENT_USER_GUIDE.md", "FOREIGN_USER_GUIDE.md", "CHAT_USER_GUIDE.md",
                   "AGENTCTL_SKILL.md"),
        required_dependencies=(),
        # `start --slot` runs the wrkslots command-line tool (an executable found on PATH,
        # never an import), so the guide must name it.
        sibling_package_exemptions=("wrkslots",),
        command_userguides=(("herdr-agent", "USER_GUIDE.md"),
                           ("herdr-subagents", "USER_GUIDE.md"), ("herdr-chat", "CHAT_USER_GUIDE.md")),
        package_owned_docs=True,
    ),
    Project(
        directory="wrkslots",
        distribution="wrkslots",
        package="wrkslots",
        commands=("wrkslots",),
        # `validate-cargo-*` is a public managed-directory spelling.
        doc_term_exemptions=("cargo",),
        resources=(
            "README.md",
            "USER_GUIDE.md",
            "py.typed",
            "examples/liveness_probe.py",
        ),
        required_dependencies=(),
        # A coordinator box (`wrkslots box`) keeps the agent launcher's registry writable
        # (a directory it names, never an import), so the guide names that launcher.
        sibling_package_exemptions=("agentctl",),
    ),
)

_DIST_SEP = re.compile(r"[-_.]+")
_FOREIGN_DOC_TERMS = re.compile(
    r"\b(?:cargo|crate|crates\.io|rust|rustc|rustup)\b", re.IGNORECASE
)
#: How each command -- and each ``python -m`` module -- is asked to print its own user guide.
#:
#: A global ``--userguide`` flag is the norm here, but a command whose surface is
#: ``<command> <subcommand>`` says it as a subcommand instead, because a documentation flag sitting
#: beside a subcommand list is the mixing that surface exists to avoid.
_USERGUIDE_INVOCATION: dict[str, tuple[str, ...]] = {
    "herdr-run": ("userguide",),
    "herdr_run": ("userguide",),
    "agentctl": ("userguide",),
}


_COMMON_DOC_TERMS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("suite name", re.compile(r"agent-utils", re.IGNORECASE)),
    ("unrelated project", re.compile(r"\b(?:DeepScry|Hermit)\b", re.IGNORECASE)),
    ("source-tree docs path", re.compile(r"common/docs/", re.IGNORECASE)),
    ("source-tree script", re.compile(r"scripts/embed_userguides\.py", re.IGNORECASE)),
    # scripts/setup-hooks.sh is not a path in this source tree (there is no such
    # file here): it is the installer wrkslots runs inside the user's own
    # repository, and its guide documents that file name and the output naming it.
    (
        "source-tree path",
        re.compile(
            r"(?:^|[ (`])(?:py/|rs/|cross/|scripts/(?!setup-hooks\.sh\b))",
            re.IGNORECASE | re.MULTILINE,
        ),
    ),
    ("unexpanded template syntax", re.compile(r"\{\{|\}\}")),
    (
        "development-history language",
        re.compile(
            r"\b(?:prototype|roadmap|formerly|previously|planned|not yet|legacy|historical|"
            r"predates?|follow-on|stub)\b",
            re.IGNORECASE,
        ),
    ),
    (
        "implementation-provenance language",
        re.compile(
            r"\b(?:ported|parity|cross-language|both builds?)\b|"
            r"\b(?:direct|generic|typed)?\s*port of\b",
            re.IGNORECASE,
        ),
    ),
)
_REQUIREMENT_NAME = re.compile(r"^\s*([A-Za-z0-9][A-Za-z0-9._-]*)")
_PUBLIC_SCAN_IGNORED = {"__pycache__", "build", "dist", "tests"}

_PublicDocNode = ast.Module | ast.ClassDef | ast.FunctionDef | ast.AsyncFunctionDef


class CheckError(RuntimeError):
    """One package failed its artifact contract."""


class _WheelArchive(Protocol):
    """The small, stable archive surface used by wheel inspection."""

    def namelist(self) -> list[str]: ...

    def read(self, name: str) -> bytes: ...


@dataclass(frozen=True)
class _WheelMetadata:
    """The normalized metadata fields the artifact contract consumes."""

    name: str
    version: str
    requires_python: str
    requirements: tuple[str, ...]
    description: str


@dataclass(frozen=True)
class _WheelInspection:
    """Installer-visible wheel data that direct and sdist builds must share."""

    version: str
    userguide: str
    resources: tuple[tuple[str, bytes], ...]


def _project_aliases() -> dict[str, Project]:
    """Return exact CLI aliases after checking the complete project registry."""

    aliases: dict[str, Project] = {}
    distributions: set[str] = set()
    directories: set[str] = set()
    packages: set[str] = set()
    commands: set[str] = set()
    for project in PROJECTS:
        if project.distribution in distributions:
            raise CheckError(
                f"project registry repeats distribution {project.distribution!r}"
            )
        if project.directory in directories:
            raise CheckError(f"project registry repeats directory {project.directory!r}")
        if project.package in packages:
            raise CheckError(f"project registry repeats package {project.package!r}")
        distributions.add(project.distribution)
        directories.add(project.directory)
        packages.add(project.package)
        for command in project.commands:
            if command in commands:
                raise CheckError(f"project registry repeats command {command!r}")
            commands.add(command)
        for alias in (project.distribution, project.directory):
            previous = aliases.get(alias)
            if previous is not None and previous != project:
                raise CheckError(
                    f"project alias {alias!r} names both {previous.distribution!r} "
                    f"and {project.distribution!r}"
                )
            aliases[alias] = project
    return aliases


def _select_projects(names: list[str]) -> tuple[Project, ...]:
    """Resolve strict, repeatable selectors in canonical registry order."""

    aliases = _project_aliases()
    if not names:
        return PROJECTS
    selected: dict[Project, str] = {}
    for name in names:
        project = aliases.get(name)
        if project is None:
            choices = ", ".join(sorted(aliases))
            raise CheckError(f"unknown project {name!r}; choose one of: {choices}")
        previous = selected.get(project)
        if previous is not None:
            raise CheckError(
                f"project {project.distribution!r} selected more than once "
                f"({previous!r}, {name!r})"
            )
        selected[project] = name
    return tuple(project for project in PROJECTS if project in selected)


def _check_project_topology() -> None:
    """Require every project manifest to have exactly one registry entry."""

    registered = {project.directory for project in PROJECTS}
    observed = {
        path.parent.name
        for path in PY_ROOT.glob("*/pyproject.toml")
        if path.is_file()
    }
    missing = sorted(registered - observed)
    if missing:
        raise CheckError(f"project registry names directories without manifests: {missing}")
    unregistered = sorted(observed - registered)
    if unregistered:
        raise CheckError(
            f"publishable project manifests are missing from the registry: {unregistered}"
        )


def _argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument(
        "--project",
        action="append",
        default=[],
        metavar="NAME",
        help=(
            "check only this distribution or source-directory alias; repeat for "
            "multiple projects (default: all projects)"
        ),
    )
    selection.add_argument(
        "--list-projects",
        action="store_true",
        help="list accepted project names and directory aliases, then exit",
    )
    return parser


def _public_doc_nodes(tree: ast.Module) -> list[_PublicDocNode]:
    nodes: list[_PublicDocNode] = [tree]

    def add_public_members(body: list[ast.stmt]) -> None:
        for node in body:
            if not isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            if node.name.startswith("_"):
                continue
            nodes.append(node)
            if isinstance(node, ast.ClassDef):
                add_public_members(node.body)

    add_public_members(tree.body)
    return nodes


def _check_public_docstrings(projects: tuple[Project, ...] | None = None) -> None:
    """Keep every published package's introspection docs standalone."""

    for project in PROJECTS if projects is None else projects:
        package = PY_ROOT / project.directory
        for path in sorted(package.rglob("*.py")):
            relative = path.relative_to(package)
            if any(
                part in _PUBLIC_SCAN_IGNORED or part.endswith(".egg-info")
                for part in relative.parts
            ):
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            for node in _public_doc_nodes(tree):
                docstring = ast.get_docstring(node, clean=False)
                if docstring is None:
                    name = getattr(node, "name", "<module>")
                    line = getattr(node, "lineno", 1)
                    raise CheckError(
                        f"{project.distribution}: public API {relative}:{line} "
                        f"({name}) has no docstring"
                    )
                errors = _doc_violations(project, docstring)
                if errors:
                    name = getattr(node, "name", "<module>")
                    line = getattr(node, "lineno", 1)
                    raise CheckError(
                        f"{project.distribution}: public docstring "
                        f"{relative}:{line} "
                        f"({name}) is not standalone: {'; '.join(errors)}"
                    )


def _normalized_distribution(value: str) -> str:
    return _DIST_SEP.sub("-", value).lower()


def _require_build_backend() -> None:
    """Fail early with an actionable error when the offline backend is absent."""

    try:
        importlib.import_module("setuptools.build_meta")
        setuptools_version = importlib.metadata.version("setuptools")
    except (ImportError, importlib.metadata.PackageNotFoundError) as exc:
        raise CheckError(
            "setuptools.build_meta is unavailable; install setuptools>=77 in the "
            "interpreter running this offline package check"
        ) from exc
    major_match = re.match(r"([0-9]+)", setuptools_version)
    if major_match is None or int(major_match.group(1)) < 77:
        raise CheckError(
            f"setuptools {setuptools_version!r} is too old; this repository requires setuptools>=77"
        )


def _requirement_name(requirement: str) -> str:
    match = _REQUIREMENT_NAME.match(requirement)
    return _normalized_distribution(match.group(1)) if match is not None else ""


def _unexpected_sibling_requirements(
    project: Project, requirement_names: set[str]
) -> list[str]:
    """Return undeclared coupling to another distribution in this repository.

    Most projects are alternative, independently installed tools and must not
    acquire an accidental sibling dependency.  A project that deliberately
    composes with a sibling names it in ``sibling_package_exemptions``; the
    exemption is therefore explicit and local to that one package.
    """

    exemptions = {
        _normalized_distribution(name) for name in project.sibling_package_exemptions
    }
    return sorted(
        sibling.distribution
        for sibling in PROJECTS
        if sibling != project
        and _normalized_distribution(sibling.distribution) in requirement_names
        and _normalized_distribution(sibling.distribution) not in exemptions
    )


def _source_resources(project: Project, source: Path) -> tuple[tuple[str, bytes], ...]:
    """Read declared resources, following authoritative repository symlinks."""

    resources: list[tuple[str, bytes]] = []
    for relative in project.resources:
        path = source / relative
        if not path.is_file():
            raise CheckError(
                f"{project.distribution}: declared source resource is missing: {relative}"
            )
        payload = path.read_bytes()
        if Path(relative).suffix.lower() in {".md", ".rst"}:
            try:
                document = payload.decode("utf-8")
            except UnicodeDecodeError as exc:
                raise CheckError(
                    f"{project.distribution}: documentation resource {relative} is not UTF-8"
                ) from exc
            errors = _doc_violations(project, document, document_name=relative)
            if errors:
                raise CheckError(
                    f"{project.distribution}: documentation resource {relative} is not standalone: "
                    f"{'; '.join(errors)}"
                )
        resources.append((relative, payload))
    return tuple(resources)


def _unexpected_package_members(
    names: set[str],
    package_prefix: str,
    source_modules: set[str],
    resources: tuple[str, ...],
) -> list[str]:
    """Return shipped package members that are neither source nor declared resources."""

    expected = source_modules | {f"{package_prefix}{resource}" for resource in resources}
    return sorted(
        name
        for name in names
        if name.startswith(package_prefix) and not name.endswith("/") and name not in expected
    )


def _doc_violations(project: Project, text: str, *, document_name: str | None = None) -> list[str]:
    errors: list[str] = []
    checked_text = text
    if project.distribution == "dagrun":
        for value in ("cargo-build", "cargo-test", "cargo-nextest"):
            checked_text = checked_text.replace(value, " " * len(value))
    for foreign in _FOREIGN_DOC_TERMS.finditer(checked_text):
        # The unified operator guide deliberately names the distributions whose
        # supported adapters differ. This does not waive public API/README lint.
        if (project.package == "agentctl" and foreign.group(0).lower() == "rust"
                and document_name in ("USER_GUIDE.md", "AGENT_USER_GUIDE.md", "FOREIGN_USER_GUIDE.md")):
            continue
        if foreign.group(0).lower() not in project.doc_term_exemptions:
            errors.append(f"foreign-language term {foreign.group(0)!r}")
    for description, pattern in _COMMON_DOC_TERMS:
        match = pattern.search(text)
        if match is not None:
            errors.append(f"{description} {match.group(0)!r}")
    for sibling in PROJECTS:
        if sibling == project:
            continue
        for name in (sibling.distribution, sibling.package):
            if name.lower() in project.sibling_package_exemptions:
                continue
            match = re.search(re.escape(name), text, re.IGNORECASE)
            if match is not None:
                errors.append(f"sibling package {match.group(0)!r}")
                break
    return errors


def _offline_env() -> dict[str, str]:
    env = dict(os.environ)
    env.update(
        {
            "PIP_DISABLE_PIP_VERSION_CHECK": "1",
            "PIP_NO_INDEX": "1",
            "PIP_NO_INPUT": "1",
            "PYTHONDONTWRITEBYTECODE": "1",
            "NO_COLOR": "1",
        }
    )
    return env


def _run(
    argv: list[str],
    *,
    env: dict[str, str],
    cwd: Path | None = None,
    timeout: int = 120,
) -> subprocess.CompletedProcess[str]:
    try:
        proc = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise CheckError(f"command timed out after {timeout}s: {' '.join(argv)}") from exc
    if proc.returncode != 0:
        command = " ".join(argv)
        raise CheckError(
            f"command failed ({proc.returncode}): {command}\n"
            f"stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    return proc


def _copy_project(project: Project, destination: Path) -> Path:
    source = PY_ROOT / project.directory
    manifest = source / "pyproject.toml"
    if not manifest.is_file():
        raise CheckError(f"{source}: missing pyproject.toml")
    for relative in ("README.md", "USER_GUIDE.md", "LICENSE"):
        linked = source / relative
        if project.package_owned_docs and relative != "LICENSE" and linked.is_file():
            continue
        if not linked.is_symlink() or not linked.resolve().is_file():
            raise CheckError(
                f"{project.distribution}: source {relative} must be a valid authoritative symlink"
            )
    manifest_text = manifest.read_text(encoding="utf-8")
    foreign = _FOREIGN_DOC_TERMS.search(manifest_text)
    if foreign is not None:
        raise CheckError(
            f"{project.distribution}: pyproject.toml contains foreign-language term "
            f"{foreign.group(0)!r}"
        )
    target = destination / project.directory
    shutil.copytree(
        source,
        target,
        ignore=shutil.ignore_patterns(
            "__pycache__", "*.pyc", "*.pyo", "*.egg-info", "build", "dist"
        ),
    )
    return target


def _build_wheel(project: Project, source: Path, wheel_root: Path) -> Path:
    before = set(wheel_root.glob("*.whl"))
    _run(
        [
            sys.executable,
            "-m",
            "pip",
            "wheel",
            "--no-deps",
            "--no-build-isolation",
            "--wheel-dir",
            str(wheel_root),
            str(source),
        ],
        env=_offline_env(),
    )
    created = set(wheel_root.glob("*.whl")) - before
    if len(created) != 1:
        raise CheckError(
            f"{project.distribution}: expected exactly one wheel, found {sorted(created)}"
        )
    return created.pop()


def _build_sdist(project: Project, source: Path, sdist_root: Path) -> Path:
    before = set(sdist_root.glob("*.tar.gz"))
    code = (
        "import sys; from setuptools.build_meta import build_sdist; "
        "print(build_sdist(sys.argv[1]))"
    )
    _run(
        [sys.executable, "-c", code, str(sdist_root)],
        env=_offline_env(),
        cwd=source,
    )
    created = set(sdist_root.glob("*.tar.gz")) - before
    if len(created) != 1:
        raise CheckError(
            f"{project.distribution}: expected exactly one sdist, found {sorted(created)}"
        )
    return created.pop()


def _inspect_sdist(project: Project, source: Path, archive: Path) -> None:
    source_resources = _source_resources(project, source)
    with tarfile.open(archive, mode="r:gz") as package:
        members = package.getmembers()
        links = sorted(member.name for member in members if member.issym() or member.islnk())
        if links:
            raise CheckError(
                f"{project.distribution}: sdist contains repository links instead of files: {links}"
            )
        roots = {member.name.split("/", 1)[0] for member in members if member.name}
        if len(roots) != 1:
            raise CheckError(f"{project.distribution}: sdist has unexpected roots {sorted(roots)}")
        prefix = f"{next(iter(roots))}/"
        by_name = {member.name: member for member in members}
        expected = {
            f"{prefix}pyproject.toml",
            f"{prefix}PKG-INFO",
            f"{prefix}LICENSE",
            *(f"{prefix}{resource}" for resource in project.resources),
        }
        for path in source.rglob("*.py"):
            relative = path.relative_to(source)
            if any(
                part in _PUBLIC_SCAN_IGNORED or part.endswith(".egg-info")
                for part in relative.parts
            ):
                continue
            expected.add(f"{prefix}{relative.as_posix()}")
        missing = sorted(expected - set(by_name))
        if missing:
            raise CheckError(f"{project.distribution}: sdist is missing {missing}")
        unexpected_documents = sorted(
            member.name
            for member in members
            if member.isfile()
            and Path(member.name).suffix.lower() in {".md", ".rst"}
            and member.name not in expected
        )
        if unexpected_documents:
            raise CheckError(
                f"{project.distribution}: sdist contains undeclared documentation resources "
                f"{unexpected_documents}"
            )
        source_artifacts = (("LICENSE", (source / "LICENSE").read_bytes()), *source_resources)
        for artifact_relative, source_bytes in source_artifacts:
            name = f"{prefix}{artifact_relative}"
            member = by_name[name]
            if not member.isfile():
                raise CheckError(
                    f"{project.distribution}: sdist {artifact_relative} is not a regular file"
                )
            extracted = package.extractfile(member)
            if extracted is None:
                raise CheckError(
                    f"{project.distribution}: could not read sdist {artifact_relative}"
                )
            artifact_bytes = extracted.read()
            if artifact_bytes != source_bytes:
                raise CheckError(
                    f"{project.distribution}: sdist {artifact_relative} differs from its "
                    "authoritative source"
                )


def _metadata(archive: _WheelArchive) -> tuple[str, _WheelMetadata]:
    paths = [name for name in archive.namelist() if name.endswith(".dist-info/METADATA")]
    if len(paths) != 1:
        raise CheckError(f"wheel has {len(paths)} METADATA files (expected one)")
    raw = archive.read(paths[0])
    parsed = email.parser.BytesParser(policy=email.policy.default).parsebytes(raw)
    if not isinstance(parsed, email.message.EmailMessage):
        raise CheckError("could not parse wheel METADATA")
    metadata = _WheelMetadata(
        name=str(parsed.get("Name", "")),
        version=str(parsed.get("Version", "")),
        requires_python=str(parsed.get("Requires-Python", "")),
        requirements=tuple(str(value) for value in parsed.get_all("Requires-Dist", [])),
        description=parsed.get_content(),
    )
    return paths[0].removesuffix("METADATA"), metadata


def _entry_points(archive: _WheelArchive, dist_info: str) -> dict[str, str]:
    path = f"{dist_info}entry_points.txt"
    try:
        text = archive.read(path).decode("utf-8")
    except KeyError as exc:
        raise CheckError("wheel is missing entry_points.txt") from exc
    parser = configparser.ConfigParser(interpolation=None)
    parser.read_string(text)
    return dict(parser.items("console_scripts")) if parser.has_section("console_scripts") else {}


def _inspect_wheel(project: Project, wheel: Path) -> _WheelInspection:
    with zipfile.ZipFile(wheel) as archive:
        names = set(archive.namelist())
        linked_members = sorted(
            info.filename
            for info in archive.infolist()
            if stat.S_IFMT(info.external_attr >> 16) == stat.S_IFLNK
        )
        if linked_members:
            raise CheckError(
                f"{project.distribution}: wheel contains repository links instead of files: "
                f"{linked_members}"
            )
        dist_info, metadata = _metadata(archive)

        actual_name = metadata.name
        if _normalized_distribution(actual_name) != _normalized_distribution(
            project.distribution
        ):
            raise CheckError(
                f"{wheel.name}: metadata name {actual_name!r}, expected {project.distribution!r}"
            )
        version = metadata.version
        if not version:
            raise CheckError(f"{project.distribution}: empty metadata version")
        requires_python = metadata.requires_python
        if not requires_python:
            raise CheckError(f"{project.distribution}: metadata has no Requires-Python")

        requirements = list(metadata.requirements)
        requirement_names = {_requirement_name(requirement) for requirement in requirements}
        runtime_requirement_names = {
            _requirement_name(requirement)
            for requirement in requirements
            if "extra ==" not in requirement.lower()
        }
        missing_requirements = sorted(
            set(project.required_dependencies) - runtime_requirement_names
        )
        unexpected_requirements = sorted(
            runtime_requirement_names - set(project.required_dependencies)
        )
        if missing_requirements or unexpected_requirements:
            raise CheckError(
                f"{project.distribution}: runtime dependencies "
                f"{sorted(runtime_requirement_names)}, "
                f"expected {sorted(project.required_dependencies)}"
            )
        sibling_requirements = _unexpected_sibling_requirements(project, requirement_names)
        if sibling_requirements:
            raise CheckError(
                f"{project.distribution}: metadata depends on sibling package(s) "
                f"{sibling_requirements}"
            )

        description_errors = _doc_violations(project, metadata.description)
        if description_errors:
            raise CheckError(
                f"{project.distribution}: wheel metadata README is not standalone: "
                f"{'; '.join(description_errors)}"
            )

        expected_entries = set(project.commands)
        entries = _entry_points(archive, dist_info)
        if set(entries) != expected_entries:
            raise CheckError(
                f"{project.distribution}: console scripts {sorted(entries)}, "
                f"expected {sorted(expected_entries)}"
            )
        for command, target in entries.items():
            if not target.startswith(f"{project.package}."):
                raise CheckError(
                    f"{project.distribution}: {command} points outside its package: {target}"
                )

        package_prefix = f"{project.package}/"
        artifact_resources: list[tuple[str, bytes]] = []
        source_package = PY_ROOT / project.directory
        for resource, source_bytes in _source_resources(project, source_package):
            path = f"{package_prefix}{resource}"
            if path not in names:
                raise CheckError(f"{project.distribution}: wheel is missing {path}")
            artifact_bytes = archive.read(path)
            if artifact_bytes != source_bytes:
                raise CheckError(
                    f"{project.distribution}: wheel {resource} differs from its "
                    "authoritative source"
                )
            artifact_resources.append((resource, artifact_bytes))

        source_modules: dict[str, Path] = {}
        for source_path in source_package.rglob("*.py"):
            relative = source_path.relative_to(source_package)
            if any(
                part in _PUBLIC_SCAN_IGNORED or part.endswith(".egg-info")
                for part in relative.parts
            ):
                continue
            source_modules[f"{package_prefix}{relative.as_posix()}"] = source_path
        missing_modules = sorted(set(source_modules) - names)
        if missing_modules:
            raise CheckError(
                f"{project.distribution}: wheel is missing source modules {missing_modules}"
            )
        changed_modules = sorted(
            path
            for path, source_path in source_modules.items()
            if archive.read(path) != source_path.read_bytes()
        )
        if changed_modules:
            raise CheckError(
                f"{project.distribution}: wheel modules differ from differential-tested source: "
                f"{changed_modules}"
            )
        unexpected_package_members = _unexpected_package_members(
            names, package_prefix, set(source_modules), project.resources
        )
        if unexpected_package_members:
            raise CheckError(
                f"{project.distribution}: wheel contains undeclared package resources "
                f"{unexpected_package_members}"
            )

        documents: dict[str, str] = {}
        for document in ("README.md", "USER_GUIDE.md"):
            path = f"{package_prefix}{document}"
            artifact_text = archive.read(path).decode("utf-8")
            source_text = (PY_ROOT / project.directory / document).read_text(encoding="utf-8")
            if artifact_text != source_text:
                raise CheckError(
                    f"{project.distribution}: wheel {document} differs from its authoritative source"
                )
            errors = _doc_violations(project, artifact_text, document_name=document)
            if errors:
                raise CheckError(
                    f"{project.distribution}: wheel {document} is not standalone: "
                    f"{'; '.join(errors)}"
                )
            documents[document] = artifact_text
        if metadata.description != documents["README.md"]:
            raise CheckError(
                f"{project.distribution}: metadata long description differs from packaged README.md"
            )

        foreign_packages = {
            other.package
            for other in PROJECTS
            if other.package != project.package
            and any(name.startswith(f"{other.package}/") for name in names)
        }
        if foreign_packages:
            raise CheckError(
                f"{project.distribution}: wheel leaks sibling packages {sorted(foreign_packages)}"
            )
        if any(name == "ci_hub_check_outcome.py" for name in names):
            raise CheckError("pr-landing-planner authority leaked as an unpackaged top-level shim")

        license_paths = [
            name
            for name in names
            if name.startswith(dist_info)
            and name.lower().endswith(("/license", "/license.txt", "/copying"))
        ]
        if not license_paths:
            raise CheckError(f"{project.distribution}: wheel contains no license file")
        authoritative_license = (REPO_ROOT / "LICENSE").read_bytes()
        if not any(archive.read(path) == authoritative_license for path in license_paths):
            raise CheckError(
                f"{project.distribution}: wheel license differs from the authoritative license"
            )
        return _WheelInspection(
            version=version,
            userguide=documents["USER_GUIDE.md"],
            resources=tuple(artifact_resources),
        )


_SITE_CUSTOMIZE = '''\
"""Network-denial guard for artifact startup checks."""
import socket
import urllib.request

def _blocked(*args, **kwargs):
    raise RuntimeError("network access is forbidden during package smoke tests")

class _OfflineSocket(socket.socket):
    def connect(self, *args, **kwargs):
        return _blocked(*args, **kwargs)
    def connect_ex(self, *args, **kwargs):
        _blocked(*args, **kwargs)
        return 1

socket.socket = _OfflineSocket
socket.create_connection = _blocked
urllib.request.urlopen = _blocked
'''


def _venv_paths(root: Path) -> tuple[Path, Path]:
    bindir = root / ("Scripts" if os.name == "nt" else "bin")
    python = bindir / ("python.exe" if os.name == "nt" else "python")
    return bindir, python


def _smoke_wheel(
    project: Project,
    wheel: Path,
    version: str,
    userguide: str,
    resources: tuple[tuple[str, bytes], ...],
    smoke_root: Path,
) -> None:
    environment_root = smoke_root / project.directory
    # The package under test needs a fresh environment; it does not need a private copy of pip.
    # Bootstrapping pip separately into all six environments spends most of this check installing
    # the same validation tool.  The validator's pip can target each empty environment directly,
    # while the import-path assertion below still proves the wheel landed in that environment.
    venv.EnvBuilder(with_pip=False, clear=True).create(environment_root)
    bindir, python = _venv_paths(environment_root)
    env = _offline_env()
    run_root = smoke_root / "work" / project.directory
    run_root.mkdir(parents=True)

    guard = environment_root / "offline_guard"
    guard.mkdir()
    (guard / "sitecustomize.py").write_text(_SITE_CUSTOMIZE, encoding="utf-8")
    env["PYTHONPATH"] = str(guard)

    _run(
        [
            sys.executable,
            "-m",
            "pip",
            "--python",
            str(python),
            "install",
            "--no-index",
            "--no-deps",
            str(wheel),
        ],
        env=env,
        cwd=run_root,
    )

    sibling_assertions = "; ".join(
        f"assert importlib.util.find_spec({other.package!r}) is None"
        for other in PROJECTS
        if other.package != project.package
    )
    version_code = (
        "import importlib.util; from pathlib import Path; "
        "from importlib.metadata import version; "
        f"import {project.package} as package; "
        f"assert version({project.distribution!r}) == {version!r}; "
        f"assert package.__version__ == {version!r}; "
        f"assert Path(package.__file__).resolve().is_relative_to(Path({str(environment_root)!r}).resolve()); "
        f"{sibling_assertions}"
    )
    _run([str(python), "-c", version_code], env=env, cwd=run_root)

    module_guide_args = list(_USERGUIDE_INVOCATION.get(project.package, ("--userguide",)))
    module_invocations = (["--help"], ["--version"], module_guide_args)
    for args in module_invocations:
        result = _run(
            [str(python), "-m", project.package, *args],
            env=env,
            cwd=run_root,
            timeout=30,
        )
        combined = result.stdout + result.stderr
        if "Traceback (most recent call last)" in combined:
            raise CheckError(f"python -m {project.package} {' '.join(args)} emitted a traceback")
        if args == ["--version"] and version not in combined:
            raise CheckError(
                f"python -m {project.package} --version did not report {version!r}: {combined!r}"
            )
        if args == module_guide_args and (result.stdout != userguide or result.stderr):
            raise CheckError(
                f"python -m {project.package} {' '.join(module_guide_args)} differs from "
                "packaged USER_GUIDE.md"
            )

    command_userguides = dict(project.command_userguides)
    resource_text = {name: data.decode("utf-8") for name, data in resources}
    for command in project.commands:
        executable = bindir / command
        if not executable.is_file():
            raise CheckError(f"{project.distribution}: installed command missing: {executable}")
        guide_args = list(_USERGUIDE_INVOCATION.get(command, ("--userguide",)))
        invocations: list[list[str]] = [["--help"], ["--version"]]
        if command != "cpuset-alloc":
            invocations.append(guide_args)
        for command_args in invocations:
            result = _run(
                [str(executable), *command_args], env=env, cwd=run_root, timeout=30
            )
            combined = result.stdout + result.stderr
            if "Traceback (most recent call last)" in combined:
                raise CheckError(f"{command} {' '.join(command_args)} emitted a traceback")
            if command_args == ["--version"] and version not in combined:
                raise CheckError(f"{command} --version did not report {version!r}: {combined!r}")
            expected_guide_name = command_userguides.get(command, "USER_GUIDE.md")
            expected_guide = userguide if expected_guide_name == "USER_GUIDE.md" else resource_text[expected_guide_name]
            if command_args == guide_args and (
                result.stdout != expected_guide or result.stderr
            ):
                raise CheckError(
                    f"{command} {' '.join(guide_args)} differs from packaged {expected_guide_name}"
                )

    if project.package == "agentctl":
        # These invocations run in the wheel-only environment whose import
        # checks above proved every sibling, including herdr_run, absent.
        agentctl_command = str(bindir / "agentctl")
        for arguments, expected in (
            (["quickstart"], resource_text["QUICKSTART.md"]),
            (["chat", "userguide"], resource_text["CHAT_USER_GUIDE.md"]),
        ):
            result = _run([agentctl_command, *arguments], env=env, cwd=run_root)
            if result.stdout != expected or result.stderr:
                raise CheckError(f"agentctl {' '.join(arguments)} differs from packaged CLI documentation")
        for arguments in (["capabilities"], ["chat", "--help"], ["chat", "quickstart"],
                          ["chat", "init", "--help"], ["chat", "run", "--help"],
                          ["chat", "tick", "--help"], ["chat", "reply", "--help"],
                          ["start", "--help"], ["mcp", "--help"]):
            _run([agentctl_command, *arguments], env=env, cwd=run_root, timeout=30)
        _run([str(python), "-c", "import agentctl.chat, agentctl.sessions, agentctl.mcp; "
              "import agentctl.foreign.agent_runner, agentctl.foreign.mcp.server"], env=env, cwd=run_root)


def main(argv: list[str] | None = None) -> int:
    parser = _argument_parser()
    arguments = parser.parse_args(argv)
    try:
        projects = _select_projects(arguments.project)
        if arguments.list_projects:
            for project in PROJECTS:
                alias = (
                    f" (directory alias: {project.directory})"
                    if project.directory != project.distribution
                    else ""
                )
                print(f"{project.distribution}{alias}")
            return 0
        _check_project_topology()
        _require_build_backend()
        _check_public_docstrings(projects)
        index_text = (PY_ROOT / "README.md").read_text(encoding="utf-8")
        foreign = _FOREIGN_DOC_TERMS.search(index_text)
        if foreign is not None:
            raise CheckError(
                "py/README.md contains foreign-language term "
                f"{foreign.group(0)!r}"
            )
        with tempfile.TemporaryDirectory(prefix="agent-utils-python-packages-") as raw:
            root = Path(raw)
            build_root = root / "sources"
            source_wheel_root = root / "source-wheels"
            sdist_root = root / "sdists"
            sdist_wheel_root = root / "sdist-wheels"
            smoke_root = root / "venvs"
            build_root.mkdir()
            source_wheel_root.mkdir()
            sdist_root.mkdir()
            sdist_wheel_root.mkdir()
            smoke_root.mkdir()

            for project in projects:
                source = _copy_project(project, build_root)
                source_wheel = _build_wheel(project, source, source_wheel_root)
                source_result = _inspect_wheel(project, source_wheel)
                sdist = _build_sdist(project, source, sdist_root)
                _inspect_sdist(project, source, sdist)
                sdist_wheel = _build_wheel(project, sdist, sdist_wheel_root)
                sdist_result = _inspect_wheel(project, sdist_wheel)
                if sdist_result != source_result:
                    raise CheckError(
                        f"{project.distribution}: source and sdist wheels disagree in "
                        "version, user guide, or declared resource bytes"
                    )
                version = sdist_result.version
                _smoke_wheel(
                    project,
                    sdist_wheel,
                    version,
                    sdist_result.userguide,
                    sdist_result.resources,
                    smoke_root,
                )
                print(
                    f"check_python_packages: ok {project.distribution} {version} "
                    f"wheel + sdist ({', '.join(project.commands)})"
                )
    except (
        CheckError,
        OSError,
        UnicodeError,
        configparser.Error,
        subprocess.SubprocessError,
        tarfile.TarError,
        zipfile.BadZipFile,
    ) as exc:
        print(f"check_python_packages: FAIL: {exc}", file=sys.stderr)
        return 1
    if len(projects) == len(PROJECTS):
        print(f"check_python_packages: all {len(PROJECTS)} distributions passed")
    else:
        print(
            f"check_python_packages: selected {len(projects)} "
            f"distribution{'s' if len(projects) != 1 else ''} passed"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

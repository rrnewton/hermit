#!/usr/bin/env python3
"""Build and inspect independently publishable Rust crates.

The repository workspace is convenient for development, but it can hide a
crate that would be incomplete when uploaded by itself.  This check asks Cargo
to create the exact registry archive for each public crate, then inspects that
archive for its binaries, library, license, README, and embedded user guide.
It also keeps installer-facing documentation specific to the Rust package.

With no selector, every crate is checked. ``--crate`` may be repeated to check
only named crates while the global workspace topology and crate registry remain
checked. Local dependency archives required to verify selected dependents are
prepared and verified automatically, but are not smoke-tested unless selected.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent
RS_ROOT = REPO_ROOT / "rs"


@dataclass(frozen=True)
class Crate:
    name: str
    bins: tuple[str, ...]
    library: str
    command_userguides: tuple[tuple[str, str], ...]
    extra_documents: tuple[str, ...] = ()


CRATES: tuple[Crate, ...] = (
    Crate("chat-subscription", (), "chat_subscription", ()),
    Crate("chat-subscription-plugin", (), "chat_subscription_plugin", ()),
    Crate(
        "dagrun",
        ("dagrun", "cpuset-alloc"),
        "dagrun",
        (("dagrun", "src/embedded_userguide.md"),),
    ),
    Crate("tick-hub", ("tick-hub",), "tick_hub", (("tick-hub", "src/embedded_userguide.md"),)),
    Crate(
        "pr-landing-planner",
        ("pr-landing-planner",),
        "pr_landing_planner",
        (("pr-landing-planner", "src/embedded_userguide.md"),),
    ),
    Crate(
        "herdr-run",
        ("herdr-run",),
        "herdr_run",
        (("herdr-run", "src/embedded_userguide.md"),),
    ),
    Crate(
        "agentctl", ("agentctl", "herdr-agent"), "agentctl",
        (("agentctl", "src/embedded_userguide.md"), ("herdr-agent", "src/embedded_agent_userguide.md")),
        ("src/embedded_quickstart.md", "src/embedded_agentctl_skill.md"),
    ),
)

# Cargo rewrites versioned path dependencies to registry dependencies while verifying an archive.
# These libraries are released in dependency order, so the first local package check necessarily
# precedes their registry availability. Dependents are patched to sources extracted from the exact
# already-inspected dependency archives, never to the live checkout.
LOCAL_PACKAGE_PATCHES: dict[str, tuple[str, ...]] = {
    "chat-subscription-plugin": ("chat-subscription",),
    "agentctl": ("chat-subscription", "chat-subscription-plugin"),
}

_FOREIGN_DOC_TERMS = re.compile(
    r"\bpython(?:[0-9]+(?:\.[0-9]+)*)?\b|\b(?:pip|pypi)\b|"
    r"\b[A-Za-z_][A-Za-z0-9_]*\.py\b|(?:^|[ (`])py/",
    re.IGNORECASE | re.MULTILINE,
)
_COMMON_DOC_TERMS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("suite name", re.compile(r"agent-utils", re.IGNORECASE)),
    ("unrelated project", re.compile(r"\b(?:DeepScry|Hermit)\b", re.IGNORECASE)),
    ("source-tree docs path", re.compile(r"common/docs/", re.IGNORECASE)),
    ("source-tree script", re.compile(r"scripts/embed_userguides\.py", re.IGNORECASE)),
    (
        "source-tree path",
        re.compile(r"(?:^|[ (`])(?:py|rs|scripts|cross)/", re.IGNORECASE | re.MULTILINE),
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


#: How each command is asked to print its own user guide.
#:
#: A global `--userguide` flag is the norm here, but a command whose surface is
#: `<command> <subcommand>` says it as a subcommand instead, because a documentation flag sitting
#: beside a subcommand list is the mixing that surface exists to avoid.
_USERGUIDE_INVOCATION: dict[str, tuple[str, ...]] = {"herdr-run": ("userguide",), "agentctl": ("userguide",)}


class CheckError(RuntimeError):
    """A crate failed its publishable-artifact contract."""


def _crate_registry() -> dict[str, Crate]:
    """Return the crate registry after checking names and dependency topology."""

    crates: dict[str, Crate] = {}
    libraries: set[str] = set()
    binaries: set[str] = set()
    positions: dict[str, int] = {}
    for position, crate in enumerate(CRATES):
        if crate.name in crates:
            raise CheckError(f"crate registry repeats name {crate.name!r}")
        if crate.library in libraries:
            raise CheckError(f"crate registry repeats library {crate.library!r}")
        crates[crate.name] = crate
        libraries.add(crate.library)
        positions[crate.name] = position
        for binary in crate.bins:
            if binary in binaries:
                raise CheckError(f"crate registry repeats binary {binary!r}")
            binaries.add(binary)
    unknown_subjects = sorted(set(LOCAL_PACKAGE_PATCHES) - set(crates))
    if unknown_subjects:
        raise CheckError(
            f"local package dependency map has unknown crates: {unknown_subjects}"
        )
    for subject, dependencies in LOCAL_PACKAGE_PATCHES.items():
        seen: set[str] = set()
        for dependency in dependencies:
            if dependency not in crates:
                raise CheckError(
                    f"{subject}: local package dependency {dependency!r} is not registered"
                )
            if dependency in seen:
                raise CheckError(
                    f"{subject}: local package dependency {dependency!r} is repeated"
                )
            seen.add(dependency)
            if positions[dependency] >= positions[subject]:
                raise CheckError(
                    f"{subject}: local package dependency {dependency!r} must precede its "
                    "dependent in the crate registry"
                )
    return crates


def _select_crates(names: list[str]) -> tuple[Crate, ...]:
    """Resolve strict, repeatable selectors in canonical registry order."""

    registry = _crate_registry()
    if not names:
        return CRATES
    selected: set[str] = set()
    for name in names:
        if name not in registry:
            choices = ", ".join(sorted(registry))
            raise CheckError(f"unknown crate {name!r}; choose one of: {choices}")
        if name in selected:
            raise CheckError(f"crate {name!r} selected more than once")
        selected.add(name)
    return tuple(crate for crate in CRATES if crate.name in selected)


def _crate_work(
    selected: tuple[Crate, ...],
) -> tuple[tuple[Crate, ...], dict[str, tuple[str, ...]]]:
    """Return selected crates plus the dependency archives their verification needs."""

    _crate_registry()
    selected_names = {crate.name for crate in selected}
    required_by: dict[str, set[str]] = {}

    def add_dependencies(crate_name: str, requested_name: str) -> None:
        for dependency in LOCAL_PACKAGE_PATCHES.get(crate_name, ()):
            if dependency not in selected_names:
                required_by.setdefault(dependency, set()).add(requested_name)
            add_dependencies(dependency, requested_name)

    for crate in selected:
        add_dependencies(crate.name, crate.name)
    work_names = selected_names | set(required_by)
    work = tuple(crate for crate in CRATES if crate.name in work_names)
    reasons = {
        name: tuple(sorted(requesters)) for name, requesters in required_by.items()
    }
    return work, reasons


def _argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument(
        "--crate",
        action="append",
        default=[],
        metavar="NAME",
        help="check only this crate; repeat for multiple crates (default: all crates)",
    )
    selection.add_argument(
        "--list-crates",
        action="store_true",
        help="list accepted crate names, then exit",
    )
    return parser


def _check_public_rustdoc(crates: tuple[Crate, ...] | None = None) -> None:
    """Keep every published crate's public rustdoc standalone."""

    for crate in CRATES if crates is None else crates:
        root = RS_ROOT / crate.name
        for path in sorted((root / "src").rglob("*.rs")):
            in_block_doc = False
            for line_number, line in enumerate(
                path.read_text(encoding="utf-8").splitlines(), start=1
            ):
                stripped = line.lstrip()
                line_doc = stripped.startswith(("//!", "///"))
                if stripped.startswith(("/*!", "/**")):
                    in_block_doc = True
                if line_doc or in_block_doc:
                    errors = _doc_violations(crate, stripped)
                    if errors:
                        raise CheckError(
                            f"{crate.name}: rustdoc {path.relative_to(root)}:{line_number} "
                            f"is not standalone: {'; '.join(errors)}"
                        )
                if in_block_doc and "*/" in stripped:
                    in_block_doc = False


def _check_missing_docs(crates: tuple[Crate, ...] | None = None) -> None:
    """Ask rustc to reject every undocumented public library item."""

    env = dict(os.environ)
    existing = env.get("RUSTFLAGS", "").strip()
    env["RUSTFLAGS"] = f"{existing} -D missing-docs".strip()
    command = ["cargo", "check", "--locked", "--offline"]
    if crates is None:
        command.append("--workspace")
    else:
        for crate in crates:
            command.extend(("--package", crate.name))
    command.extend(("--lib", "--manifest-path", str(RS_ROOT / "Cargo.toml")))
    _run(command, env=env)


def _check_workspace_topology() -> None:
    """Require every workspace member to be checked here or explicitly non-published."""

    result = _run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
            str(RS_ROOT / "Cargo.toml"),
        ]
    )
    payload: object = json.loads(result.stdout)
    if not isinstance(payload, dict) or not isinstance(payload.get("packages"), list):
        raise CheckError("Rust workspace metadata has no package list")
    checked = {crate.name for crate in CRATES}
    observed: set[str] = set()
    for package in payload["packages"]:
        if not isinstance(package, dict) or not isinstance(package.get("name"), str):
            raise CheckError("Rust workspace metadata contains an invalid package")
        name = package["name"]
        observed.add(name)
        publish = package.get("publish")
        if name in checked and publish == []:
            raise CheckError(f"{name}: package gate lists a non-published crate")
        if name not in checked and publish != []:
            raise CheckError(
                f"{name}: publishable workspace member is missing from the package gate"
            )
    missing = sorted(checked - observed)
    if missing:
        raise CheckError(f"package gate names absent workspace members: {missing}")


def _doc_violations(crate: Crate, text: str, *, document_name: str | None = None) -> list[str]:
    errors: list[str] = []
    for foreign in _FOREIGN_DOC_TERMS.finditer(text):
        # Only the shared operator guide may name the distribution providing
        # optional adapters. Public rustdoc and other package docs stay scoped.
        if (crate.name == "agentctl" and foreign.group(0).lower() == "python"
                and document_name in ("src/embedded_userguide.md", "src/embedded_agent_userguide.md")):
            continue
        errors.append(f"foreign-package term {foreign.group(0)!r}")
    for description, pattern in _COMMON_DOC_TERMS:
        match = pattern.search(text)
        if match is not None:
            errors.append(f"{description} {match.group(0)!r}")
    sibling_text = text
    machine_values: tuple[str, ...] = ()
    if crate.name == "agentctl":
        # These are serialized capability/protocol identities, not package references. Remove the
        # complete machine values only; a prose mention of either library remains a violation.
        machine_values = (
            "chat-subscription.example",
            "chat-subscription.google-chat.workspace-events",
            "chat-subscription.google-chat.polling",
            "agentctl-chat-subscription",
            "https://docs.rs/chat-subscription-plugin/0.1.0/chat_subscription_plugin/",
        )
    elif crate.name == "chat-subscription-plugin":
        machine_values = ("agentctl-chat-subscription",)
    for machine_value in machine_values:
        sibling_text = sibling_text.replace(machine_value, " " * len(machine_value))
    for sibling in CRATES:
        if sibling == crate:
            continue
        for name in (sibling.name, sibling.library):
            match = re.search(re.escape(name), sibling_text, re.IGNORECASE)
            if match is not None:
                errors.append(f"sibling package {match.group(0)!r}")
                break
    return errors


def _run(
    argv: list[str],
    *,
    cwd: Path = REPO_ROOT,
    env: dict[str, str] | None = None,
    timeout: int = 300,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
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
    if result.returncode != 0:
        raise CheckError(
            f"command failed ({result.returncode}): {' '.join(argv)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def _metadata(crate: Crate) -> tuple[str, set[str], set[str]]:
    manifest = RS_ROOT / crate.name / "Cargo.toml"
    linked_resources = {
        "README.md",
        "LICENSE",
        *(relative for _command, relative in crate.command_userguides),
        *crate.extra_documents,
    }
    for relative in sorted(linked_resources):
        linked = RS_ROOT / crate.name / relative
        if not linked.is_symlink() or not linked.resolve().is_file():
            raise CheckError(
                f"{crate.name}: source {relative} must be a valid authoritative symlink"
            )
    result = _run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
            str(manifest),
        ]
    )
    payload: object = json.loads(result.stdout)
    if not isinstance(payload, dict):
        raise CheckError(f"{crate.name}: cargo metadata root is not an object")
    packages = payload.get("packages")
    if not isinstance(packages, list):
        raise CheckError(f"{crate.name}: cargo metadata has no package list")
    wanted_manifest = str(manifest.resolve())
    matches = [
        package
        for package in packages
        if isinstance(package, dict) and package.get("manifest_path") == wanted_manifest
    ]
    if len(matches) != 1:
        raise CheckError(f"{crate.name}: expected metadata for exactly one matching package")
    package = matches[0]
    if package.get("name") != crate.name:
        raise CheckError(f"{crate.name}: package metadata has name {package.get('name')!r}")
    version = package.get("version")
    if not isinstance(version, str) or not version:
        raise CheckError(f"{crate.name}: package metadata has no version")
    targets = package.get("targets")
    if not isinstance(targets, list):
        raise CheckError(f"{crate.name}: package metadata has no targets")
    bins: set[str] = set()
    libraries: set[str] = set()
    for target in targets:
        if not isinstance(target, dict):
            continue
        name = target.get("name")
        kinds = target.get("kind")
        if not isinstance(name, str) or not isinstance(kinds, list):
            continue
        if "bin" in kinds:
            bins.add(name)
        if "lib" in kinds:
            libraries.add(name)
    return version, bins, libraries


def _extract_dependency_archive(
    dependency: str, archive: Path, target_root: Path
) -> Path:
    destination = target_root / "package-dependencies" / dependency
    if destination.exists():
        raise CheckError(f"{dependency}: dependency archive was extracted more than once")
    destination.mkdir(parents=True)
    with tarfile.open(archive, mode="r:gz") as package:
        members = package.getmembers()
        roots: set[str] = set()
        for member in members:
            relative = PurePosixPath(member.name)
            if relative.is_absolute() or not relative.parts or ".." in relative.parts:
                raise CheckError(
                    f"{dependency}: dependency archive contains unsafe path {member.name!r}"
                )
            roots.add(relative.parts[0])
            if not member.isfile() and not member.isdir():
                raise CheckError(
                    f"{dependency}: dependency archive contains non-file {member.name!r}"
                )
            target = destination.joinpath(*relative.parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            source = package.extractfile(member)
            if source is None:
                raise CheckError(
                    f"{dependency}: cannot read dependency archive member {member.name!r}"
                )
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(source.read())
            target.chmod(member.mode & 0o777)
    if len(roots) != 1:
        raise CheckError(
            f"{dependency}: dependency archive has unexpected roots {sorted(roots)}"
        )
    root_name = next(iter(roots))
    if not root_name.startswith(f"{dependency}-"):
        raise CheckError(
            f"{dependency}: dependency archive root {root_name!r} does not match its package"
        )
    source_root = destination / root_name
    if not (source_root / "Cargo.toml").is_file():
        raise CheckError(f"{dependency}: extracted dependency has no Cargo.toml")
    return source_root


def _package_patch_arguments(
    crate: Crate,
    dependency_archives: dict[str, Path],
    target_root: Path,
) -> list[str]:
    arguments: list[str] = []
    for dependency in LOCAL_PACKAGE_PATCHES.get(crate.name, ()):
        archive = dependency_archives.get(dependency)
        if archive is None:
            raise CheckError(
                f"{crate.name}: dependency {dependency} was not packaged and inspected first"
            )
        extraction_root = target_root / "package-patches" / crate.name
        path = _extract_dependency_archive(dependency, archive, extraction_root)
        arguments.extend(
            (
                "--config",
                f"patch.crates-io.{dependency}.path={json.dumps(str(path))}",
            )
        )
    return arguments


def _package_creation_patch_arguments(crate: Crate) -> list[str]:
    """Satisfy Cargo's first-publication lookup without compiling a live dependency."""

    arguments: list[str] = []
    for dependency in LOCAL_PACKAGE_PATCHES.get(crate.name, ()):
        arguments.extend(
            (
                "--config",
                f"patch.crates-io.{dependency}.path={json.dumps(str(RS_ROOT / dependency))}",
            )
        )
    return arguments


def _package(
    crate: Crate,
    version: str,
    target_root: Path,
) -> Path:
    manifest = RS_ROOT / crate.name / "Cargo.toml"
    command = [
        "cargo",
        "package",
        "--allow-dirty",
        # Cargo cannot resolve a registry dependency before its first publication. Verification is
        # performed explicitly below against this exact archive and exact prior archives.
        "--no-verify",
        "--locked",
        "--offline",
        "--target-dir",
        str(target_root),
        "--manifest-path",
        str(manifest),
    ]
    # `--no-verify` means this resolution shim is never compiled. The resulting subject archive is
    # inspected, extracted, and then compiled below with dependencies extracted from prior exact
    # archives. Keeping creation locked also proves this metadata-only step cannot alter Cargo.lock.
    command.extend(_package_creation_patch_arguments(crate))
    _run(command)
    archive = target_root / "package" / f"{crate.name}-{version}.crate"
    if not archive.is_file():
        raise CheckError(f"{crate.name}: Cargo reported success but {archive} is missing")
    return archive


def _verify_package_archive(
    crate: Crate,
    archive: Path,
    target_root: Path,
    dependency_archives: dict[str, Path],
) -> None:
    verification_root = target_root / "package-verification" / crate.name
    source_root = _extract_dependency_archive(crate.name, archive, verification_root / "subject")
    patch_arguments = _package_patch_arguments(
        crate, dependency_archives, verification_root / "dependencies"
    )
    # Seed resolution from the workspace lock; Cargo prunes unrelated packages for this archive.
    # The subset comparison below ensures it did not upgrade or substitute any retained package.
    (source_root / "Cargo.lock").write_bytes((RS_ROOT / "Cargo.lock").read_bytes())
    command = [
        "cargo",
        "build",
        "--all-targets",
        "--offline",
        "--target-dir",
        str(target_root),
        "--manifest-path",
        str(source_root / "Cargo.toml"),
    ]
    command.extend(patch_arguments)
    _run(command)
    _verify_lock_subset(source_root / "Cargo.lock", RS_ROOT / "Cargo.lock")
    command.insert(3, "--locked")
    _run(command)


def _lock_packages(path: Path) -> set[tuple[str, str, str | None, str | None]]:
    text = path.read_text(encoding="utf-8")
    marker = "\n[[package]]\n"
    if not text.startswith("# This file is automatically @generated by Cargo.") or marker not in text:
        raise CheckError(f"invalid Cargo lockfile: {path}")
    packages = set()
    for block in text.split(marker)[1:]:
        fields: dict[str, str] = {}
        for key in ("name", "version", "source", "checksum"):
            match = re.search(rf'^{key} = "([^"]+)"$', block, re.MULTILINE)
            if match is not None:
                fields[key] = match.group(1)
        if "name" not in fields or "version" not in fields:
            raise CheckError(f"invalid Cargo package block in {path}")
        packages.add(
            (
                fields["name"],
                fields["version"],
                fields.get("source"),
                fields.get("checksum"),
            )
        )
    return packages


def _verify_lock_subset(generated: Path, workspace: Path) -> None:
    generated_packages = _lock_packages(generated)
    workspace_packages = _lock_packages(workspace)
    unexpected = sorted(generated_packages - workspace_packages)
    if unexpected:
        names = [f"{name} {version}" for name, version, _source, _checksum in unexpected]
        raise CheckError(
            "package archive resolved outside the workspace lock: " + ", ".join(names)
        )


def _inspect(crate: Crate, version: str, archive: Path) -> dict[str, str]:
    prefix = f"{crate.name}-{version}/"
    with tarfile.open(archive, mode="r:gz") as package:
        names = set(package.getnames())
        required = {
            f"{prefix}Cargo.toml",
            f"{prefix}Cargo.toml.orig",
            f"{prefix}LICENSE",
            f"{prefix}README.md",
            *(f"{prefix}{relative}" for _command, relative in crate.command_userguides),
            *(f"{prefix}{relative}" for relative in crate.extra_documents),
        }
        missing = sorted(required - names)
        if missing:
            raise CheckError(f"{crate.name}: registry archive is missing {missing}")
        members = {member.name: member for member in package.getmembers()}
        non_files = sorted(name for name in required if not members[name].isfile())
        if non_files:
            raise CheckError(
                f"{crate.name}: registry archive contains links instead of files: {non_files}"
            )
        source_root = RS_ROOT / crate.name
        source_modules = {
            f"{prefix}{path.relative_to(source_root).as_posix()}": path
            for path in (source_root / "src").rglob("*.rs")
        }
        missing_modules = sorted(set(source_modules) - names)
        if missing_modules:
            raise CheckError(f"{crate.name}: registry archive is missing {missing_modules}")
        changed_modules: list[str] = []
        for name, source_path in source_modules.items():
            member = package.extractfile(name)
            if member is None or member.read() != source_path.read_bytes():
                changed_modules.append(name)
        if changed_modules:
            raise CheckError(
                f"{crate.name}: registry modules differ from differential-tested source: "
                f"{sorted(changed_modules)}"
            )

        manifest_member = package.extractfile(f"{prefix}Cargo.toml.orig")
        if manifest_member is None:
            raise CheckError(f"{crate.name}: could not read Cargo.toml.orig")
        manifest_text = manifest_member.read().decode("utf-8")
        manifest_foreign = _FOREIGN_DOC_TERMS.search(manifest_text)
        if manifest_foreign is not None:
            raise CheckError(
                f"{crate.name}: Cargo.toml contains foreign-package term "
                f"{manifest_foreign.group(0)!r}"
            )

        documents: dict[str, str] = {}
        document_paths = {
            "README.md",
            *(relative for _command, relative in crate.command_userguides),
            *crate.extra_documents,
        }
        for relative in sorted(document_paths):
            member = package.extractfile(f"{prefix}{relative}")
            if member is None:
                raise CheckError(f"{crate.name}: could not read {relative} from registry archive")
            text = member.read().decode("utf-8")
            source_text = (RS_ROOT / crate.name / relative).read_text(encoding="utf-8")
            if text != source_text:
                raise CheckError(
                    f"{crate.name}: registry {relative} differs from its generated source"
                )
            errors = _doc_violations(crate, text, document_name=relative)
            if errors:
                raise CheckError(
                    f"{crate.name}: {relative} is not standalone: {'; '.join(errors)}"
                )
            documents[relative] = text
        license_member = package.extractfile(f"{prefix}LICENSE")
        if license_member is None:
            raise CheckError(f"{crate.name}: could not read LICENSE from registry archive")
        if license_member.read() != (REPO_ROOT / "LICENSE").read_bytes():
            raise CheckError(f"{crate.name}: registry LICENSE differs from the authoritative license")
        return documents


def _smoke(
    crate: Crate,
    version: str,
    documents: dict[str, str],
    target_root: Path,
    run_root: Path,
) -> None:
    bindir = target_root / "debug"
    suffix = ".exe" if os.name == "nt" else ""
    env = dict(os.environ)
    env["NO_COLOR"] = "1"
    run_root.mkdir()

    for binary in crate.bins:
        executable = bindir / f"{binary}{suffix}"
        if not executable.is_file():
            raise CheckError(f"{crate.name}: verified binary is missing: {executable}")
        command_guides = dict(crate.command_userguides)
        guide_args = _USERGUIDE_INVOCATION.get(binary, ("--userguide",))
        invocations: list[tuple[str, ...]] = [("--help",), ("--version",)]
        if binary in command_guides:
            invocations.append(guide_args)
        for args in invocations:
            result = _run(
                [str(executable), *args],
                cwd=run_root,
                env=env,
                timeout=30,
            )
            combined = result.stdout + result.stderr
            if args == ("--version",) and version not in combined:
                raise CheckError(
                    f"{binary} --version did not report {version!r}: {combined!r}"
                )
            if args == guide_args and (
                result.stdout != documents[command_guides[binary]] or result.stderr
            ):
                raise CheckError(
                    f"{binary} {' '.join(guide_args)} differs from packaged user guide"
                )
        if binary == "agentctl":
            result = _run([str(executable), "quickstart"], cwd=run_root, env=env, timeout=30)
            expected = documents["src/embedded_quickstart.md"]
            if result.stdout != expected or result.stderr:
                raise CheckError("agentctl quickstart differs from packaged CLI documentation")
            for arguments in (("capabilities",), ("start", "--help"), ("send", "--help"), ("stop", "--help")):
                _run([str(executable), *arguments], cwd=run_root, env=env, timeout=30)


def main(argv: list[str] | None = None) -> int:
    parser = _argument_parser()
    arguments = parser.parse_args(argv)
    try:
        selected = _select_crates(arguments.crate)
        if arguments.list_crates:
            for crate in CRATES:
                print(crate.name)
            return 0
        work, dependency_reasons = _crate_work(selected)
        selected_names = {crate.name for crate in selected}
        _check_workspace_topology()
        _check_public_rustdoc(selected)
        _check_missing_docs(selected if len(selected) != len(CRATES) else None)
        index_text = (RS_ROOT / "README.md").read_text(encoding="utf-8")
        index_match = _FOREIGN_DOC_TERMS.search(index_text)
        if index_match is not None:
            raise CheckError(
                "rs/README.md contains foreign-package term "
                f"{index_match.group(0)!r}"
            )
        with tempfile.TemporaryDirectory(prefix="agent-utils-rust-packages-") as raw:
            package_root = Path(raw)
            target_root = package_root / "target"
            smoke_root = package_root / "smoke"
            smoke_root.mkdir()
            dependency_archives: dict[str, Path] = {}
            for crate in work:
                version, bins, libraries = _metadata(crate)
                if bins != set(crate.bins):
                    raise CheckError(
                        f"{crate.name}: binary targets {sorted(bins)}, "
                        f"expected {sorted(crate.bins)}"
                    )
                if libraries != {crate.library}:
                    raise CheckError(
                        f"{crate.name}: library targets {sorted(libraries)}, "
                        f"expected {[crate.library]}"
                    )
                archive = _package(crate, version, target_root)
                documents = _inspect(crate, version, archive)
                _verify_package_archive(crate, archive, target_root, dependency_archives)
                # Only inspected and independently compiled archives become dependency inputs.
                dependency_archives[crate.name] = archive
                if crate.name in selected_names:
                    _smoke(crate, version, documents, target_root, smoke_root / crate.name)
                    print(
                        f"check_rust_packages: ok {crate.name} {version} "
                        f"({', '.join(crate.bins)})"
                    )
                else:
                    requesters = ", ".join(dependency_reasons[crate.name])
                    print(
                        f"check_rust_packages: dependency archive ok {crate.name} "
                        f"{version} (required by {requesters}; smoke skipped)"
                    )
    except (
        CheckError,
        json.JSONDecodeError,
        OSError,
        subprocess.SubprocessError,
        tarfile.TarError,
        UnicodeError,
    ) as error:
        print(f"check_rust_packages: FAIL: {error}", file=sys.stderr)
        return 1
    if len(selected) == len(CRATES):
        print(f"check_rust_packages: all {len(CRATES)} crates passed")
    else:
        dependencies = len(work) - len(selected)
        suffix = (
            f"; {dependencies} dependency archive"
            f"{'s' if dependencies != 1 else ''} prepared"
            if dependencies
            else ""
        )
        print(
            f"check_rust_packages: selected {len(selected)} "
            f"crate{'s' if len(selected) != 1 else ''} passed{suffix}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

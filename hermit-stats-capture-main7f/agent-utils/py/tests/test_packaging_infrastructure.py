from __future__ import annotations

import importlib.util
import json
import os
import re
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path
from types import ModuleType

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]


def _collect_lifecycle_tests(mark_expression: str | None = None) -> set[str]:
    command = [
        sys.executable,
        "-m",
        "pytest",
        "--collect-only",
        "-q",
        "--strict-markers",
        "-p",
        "no:cacheprovider",
        "-c",
        "pyproject.toml",
        "--rootdir=.",
        "wrkslots/tests/test_lifecycle.py",
    ]
    if mark_expression is not None:
        command.extend(("-m", mark_expression))
    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    completed = subprocess.run(
        command,
        cwd=REPO_ROOT / "py",
        env=environment,
        text=True,
        capture_output=True,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr or completed.stdout
    return {
        line
        for line in completed.stdout.splitlines()
        if line.startswith("wrkslots/tests/test_lifecycle.py::")
    }


def _load_script(name: str) -> ModuleType:
    path = REPO_ROOT / "scripts" / f"{name}.py"
    spec = importlib.util.spec_from_file_location(f"_packaging_test_{name}", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_python_package_selectors_accept_both_aliases_and_reject_duplicates() -> None:
    python_check = _load_script("check_python_packages")

    selected = python_check._select_projects(["parallel_experiment_runner", "tick-hub"])

    assert [project.distribution for project in selected] == [
        "tick-hub",
        "parallel-experiment-runner",
    ]
    assert python_check._select_projects([]) == python_check.PROJECTS
    with pytest.raises(python_check.CheckError, match="unknown project 'missing'"):
        python_check._select_projects(["missing"])
    with pytest.raises(
        python_check.CheckError,
        match="parallel-experiment-runner.*selected more than once",
    ):
        python_check._select_projects(
            ["parallel-experiment-runner", "parallel_experiment_runner"]
        )


def test_python_package_selector_list_and_help_are_discoverable(
    capsys: pytest.CaptureFixture[str],
) -> None:
    python_check = _load_script("check_python_packages")

    assert python_check.main(["--list-projects"]) == 0
    listed = capsys.readouterr().out
    assert "tick-hub (directory alias: tick_hub)" in listed
    assert "dagrun\n" in listed
    help_text = python_check._argument_parser().format_help()
    assert "--project NAME" in help_text
    assert "--list-projects" in help_text


def test_python_package_main_only_builds_selected_projects(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    python_check = _load_script("check_python_packages")
    observed: list[tuple[str, str]] = []
    inspection = python_check._WheelInspection("1.0", "guide\n", ())

    monkeypatch.setattr(
        python_check,
        "_check_project_topology",
        lambda: observed.append(("global", "topology")),
    )
    monkeypatch.setattr(python_check, "_require_build_backend", lambda: None)
    monkeypatch.setattr(
        python_check,
        "_check_public_docstrings",
        lambda projects: observed.extend(
            ("docstrings", project.distribution) for project in projects
        ),
    )
    monkeypatch.setattr(
        python_check,
        "_copy_project",
        lambda project, root: root / project.directory,
    )
    monkeypatch.setattr(
        python_check,
        "_build_wheel",
        lambda project, _source, root: root / f"{project.distribution}.whl",
    )

    def inspect_wheel(_project: object, wheel: Path) -> object:
        observed.append(("inspect", wheel.stem))
        return inspection

    monkeypatch.setattr(
        python_check,
        "_inspect_wheel",
        inspect_wheel,
    )
    monkeypatch.setattr(
        python_check,
        "_build_sdist",
        lambda project, _source, root: root / f"{project.distribution}.tar.gz",
    )
    monkeypatch.setattr(python_check, "_inspect_sdist", lambda *_args: None)
    monkeypatch.setattr(
        python_check,
        "_smoke_wheel",
        lambda project, *_args: observed.append(("smoke", project.distribution)),
    )

    assert python_check.main(["--project", "tick_hub"]) == 0
    assert observed == [
        ("global", "topology"),
        ("docstrings", "tick-hub"),
        ("inspect", "tick-hub"),
        ("inspect", "tick-hub"),
        ("smoke", "tick-hub"),
    ]


def test_python_project_topology_rejects_unregistered_and_missing_manifests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    python_check = _load_script("check_python_packages")
    registered = python_check.Project(
        directory="registered",
        distribution="registered",
        package="registered",
        commands=("registered",),
        resources=(),
        required_dependencies=(),
    )
    (tmp_path / "registered").mkdir()
    (tmp_path / "registered" / "pyproject.toml").touch()
    monkeypatch.setattr(python_check, "PY_ROOT", tmp_path)
    monkeypatch.setattr(python_check, "PROJECTS", (registered,))

    python_check._check_project_topology()
    (tmp_path / "unregistered").mkdir()
    (tmp_path / "unregistered" / "pyproject.toml").touch()
    with pytest.raises(python_check.CheckError, match="unregistered"):
        python_check._check_project_topology()

    (tmp_path / "unregistered" / "pyproject.toml").unlink()
    (tmp_path / "registered" / "pyproject.toml").unlink()
    with pytest.raises(python_check.CheckError, match="without manifests"):
        python_check._check_project_topology()


def test_rust_package_selectors_resolve_dependency_archive_closure() -> None:
    rust_check = _load_script("check_rust_packages")

    selected = rust_check._select_crates(["agentctl"])
    work, reasons = rust_check._crate_work(selected)

    assert [crate.name for crate in selected] == ["agentctl"]
    assert [crate.name for crate in work] == [
        "chat-subscription",
        "chat-subscription-plugin",
        "agentctl",
    ]
    assert reasons == {
        "chat-subscription": ("agentctl",),
        "chat-subscription-plugin": ("agentctl",),
    }
    assert rust_check._select_crates([]) == rust_check.CRATES
    with pytest.raises(rust_check.CheckError, match="unknown crate 'missing'"):
        rust_check._select_crates(["missing"])
    with pytest.raises(rust_check.CheckError, match="selected more than once"):
        rust_check._select_crates(["dagrun", "dagrun"])


def test_rust_package_selector_list_and_help_are_discoverable(
    capsys: pytest.CaptureFixture[str],
) -> None:
    rust_check = _load_script("check_rust_packages")

    assert rust_check.main(["--list-crates"]) == 0
    listed = capsys.readouterr().out.splitlines()
    assert listed == [crate.name for crate in rust_check.CRATES]
    help_text = rust_check._argument_parser().format_help()
    assert "--crate NAME" in help_text
    assert "--list-crates" in help_text


def test_rust_package_main_globally_checks_topology_but_only_smokes_selection(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    rust_check = _load_script("check_rust_packages")
    observed: list[tuple[str, str]] = []

    monkeypatch.setattr(
        rust_check,
        "_check_workspace_topology",
        lambda: observed.append(("global", "topology")),
    )
    monkeypatch.setattr(
        rust_check,
        "_check_public_rustdoc",
        lambda crates: observed.extend(("rustdoc", crate.name) for crate in crates),
    )
    monkeypatch.setattr(
        rust_check,
        "_check_missing_docs",
        lambda crates: observed.extend(
            ("missing-docs", crate.name) for crate in crates
        ),
    )
    monkeypatch.setattr(
        rust_check,
        "_metadata",
        lambda crate: ("1.0", set(crate.bins), {crate.library}),
    )
    monkeypatch.setattr(
        rust_check,
        "_package",
        lambda crate, _version, root: root / f"{crate.name}.crate",
    )
    monkeypatch.setattr(rust_check, "_inspect", lambda *_args: {})
    monkeypatch.setattr(
        rust_check,
        "_verify_package_archive",
        lambda crate, *_args: observed.append(("verify", crate.name)),
    )
    monkeypatch.setattr(
        rust_check,
        "_smoke",
        lambda crate, *_args: observed.append(("smoke", crate.name)),
    )

    assert rust_check.main(["--crate", "agentctl"]) == 0
    assert observed == [
        ("global", "topology"),
        ("rustdoc", "agentctl"),
        ("missing-docs", "agentctl"),
        ("verify", "chat-subscription"),
        ("verify", "chat-subscription-plugin"),
        ("verify", "agentctl"),
        ("smoke", "agentctl"),
    ]


def test_embed_lint_rejects_unknown_placeholders_and_sibling_packages() -> None:
    docs = _load_script("embed_userguides")
    item = docs.Render("dagrun", "README", "python", "out/README.md")

    errors = docs._lint(item, "{{UNKNOWN}}\nRust\ntick-hub\n")

    assert any("unexpanded template syntax" in error for error in errors)
    assert any("other implementation language" in error for error in errors)
    assert any("sibling package" in error for error in errors)


def test_embed_check_reports_both_staleness_and_lint(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    docs = _load_script("embed_userguides")
    item = docs.Render("dagrun", "README", "python", "out/README.md")
    template = tmp_path / item.template
    fragment = tmp_path / item.fragment
    destination = tmp_path / item.destination
    template.parent.mkdir(parents=True)
    fragment.parent.mkdir(parents=True)
    destination.parent.mkdir(parents=True)
    template.write_text("# dagrun\n\n{{DISTRIBUTION}}\n", encoding="utf-8")
    fragment.write_text("Install this distribution.\n", encoding="utf-8")
    destination.write_text("{{UNKNOWN}}\ntick-hub\n", encoding="utf-8")
    monkeypatch.setattr(docs, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(docs, "RENDERS", (item,))
    monkeypatch.setattr(docs, "STANDALONE_DOCUMENTS", ())
    monkeypatch.setattr(docs, "PACKAGE_LINKS", ())

    stale, lint_errors = docs.check()

    assert stale == [item.destination]
    assert any("unexpanded template syntax" in error for error in lint_errors)
    assert any("sibling package" in error for error in lint_errors)


def test_embed_generate_prevalidates_and_writes_only_changed_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    docs = _load_script("embed_userguides")
    first = docs.Render("dagrun", "README", "python", "out/README.md")
    second = docs.Render("dagrun", "USER_GUIDE", "python", "out/USER_GUIDE.md")
    first_template = tmp_path / first.template
    first_fragment = tmp_path / first.fragment
    first_destination = tmp_path / first.destination
    second_template = tmp_path / second.template
    first_template.parent.mkdir(parents=True)
    first_fragment.parent.mkdir(parents=True)
    first_destination.parent.mkdir(parents=True)
    second_template.parent.mkdir(parents=True, exist_ok=True)
    first_template.write_text("{{DISTRIBUTION}}\n", encoding="utf-8")
    first_fragment.write_text("valid\n", encoding="utf-8")
    first_destination.write_text("leave me alone\n", encoding="utf-8")
    second_template.write_text("{{DISTRIBUTION}}\n", encoding="utf-8")
    monkeypatch.setattr(docs, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(docs, "RENDERS", (first, second))
    monkeypatch.setattr(docs, "STANDALONE_DOCUMENTS", ())
    monkeypatch.setattr(docs, "PACKAGE_LINKS", ())

    with pytest.raises(FileNotFoundError, match="fragment missing"):
        docs.generate()

    assert first_destination.read_text(encoding="utf-8") == "leave me alone\n"
    second_fragment = tmp_path / second.fragment
    second_fragment.parent.mkdir(parents=True, exist_ok=True)
    second_fragment.write_text("also valid\n", encoding="utf-8")
    assert docs.generate() == [first.destination, second.destination]
    assert first_destination.read_text(encoding="utf-8") == "valid\n"

    old_mtime_ns = 1_600_000_000_000_000_000
    os.utime(first_destination, ns=(old_mtime_ns, old_mtime_ns))

    assert docs.generate() == []
    assert first_destination.stat().st_mtime_ns == old_mtime_ns

    first_destination.write_text("stale\n", encoding="utf-8")
    assert docs.generate() == [first.destination]
    assert first_destination.read_text(encoding="utf-8") == "valid\n"


def test_embed_check_rejects_regular_copy_and_wrong_link_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    docs = _load_script("embed_userguides")
    link = docs.PackageLink("package/README.md", "common/README.md")
    wanted = tmp_path / link.target
    destination = tmp_path / link.destination
    wanted.parent.mkdir(parents=True)
    destination.parent.mkdir(parents=True)
    wanted.write_text("authoritative\n", encoding="utf-8")
    destination.write_text("authoritative\n", encoding="utf-8")
    monkeypatch.setattr(docs, "REPO_ROOT", tmp_path)
    monkeypatch.setattr(docs, "RENDERS", ())
    monkeypatch.setattr(docs, "STANDALONE_DOCUMENTS", ())
    monkeypatch.setattr(docs, "PACKAGE_LINKS", (link,))

    stale, lint_errors = docs.check()
    assert stale == [link.destination]
    assert lint_errors == []

    destination.unlink()
    destination.symlink_to("../wrong/README.md")
    stale, _ = docs.check()
    assert stale == [link.destination]

    destination.unlink()
    destination.symlink_to(link.relative_target)
    assert docs.check() == ([], [])


def test_package_docs_and_licenses_are_authoritative_links() -> None:
    docs = _load_script("embed_userguides")

    assert len(docs.PACKAGE_LINKS) == 51
    assert {
        "rs/chat-subscription/README.md",
        "rs/chat-subscription/LICENSE",
        "rs/chat-subscription-plugin/README.md",
        "rs/chat-subscription-plugin/LICENSE",
        "py/agentctl/AGENT_USER_GUIDE.md",
        "py/agentctl/FOREIGN_USER_GUIDE.md",
        "py/agentctl/AGENTCTL_SKILL.md",
        "rs/agentctl/src/embedded_userguide.md",
        "rs/agentctl/src/embedded_quickstart.md",
        "rs/agentctl/src/embedded_agentctl_skill.md",
    } <= {link.destination for link in docs.PACKAGE_LINKS}
    assert not {
        "py/herdr_run/AGENT_USER_GUIDE.md",
        "py/herdr_run/CHAT_USER_GUIDE.md",
        "py/herdr_run/FOREIGN_USER_GUIDE.md",
        "rs/herdr-run/src/embedded_agent_userguide.md",
    } & {link.destination for link in docs.PACKAGE_LINKS}
    for link in docs.PACKAGE_LINKS:
        destination = REPO_ROOT / link.destination
        assert destination.is_symlink(), link.destination
        assert docs._link_is_current(link), link.destination


def test_artifact_doc_linters_reject_suite_language_sibling_and_template_leaks() -> None:
    python_check = _load_script("check_python_packages")
    rust_check = _load_script("check_rust_packages")

    python_errors = python_check._doc_violations(
        python_check.PROJECTS[0], "agent-utils Rust tick-hub {{UNKNOWN}}"
    )
    rust_errors = rust_check._doc_violations(
        rust_check.CRATES[0], "agent-utils Python tick-hub {{UNKNOWN}}"
    )

    for errors in (python_errors, rust_errors):
        assert any("suite name" in error for error in errors)
        assert any("foreign" in error for error in errors)
        assert any("sibling package" in error for error in errors)
        assert any("template" in error for error in errors)


def test_python_doc_lint_exemption_cannot_hide_a_later_foreign_term() -> None:
    python_check = _load_script("check_python_packages")
    project = next(
        project for project in python_check.PROJECTS if "cargo" in project.doc_term_exemptions
    )

    errors = python_check._doc_violations(
        project,
        "Cargo is supported as a target program. Install the unrelated Rust implementation.",
    )

    assert "foreign-language term 'Rust'" in errors
    assert "foreign-language term 'Cargo'" not in errors


def test_agentctl_capability_comparison_exemption_is_scoped_to_its_operator_guide() -> None:
    python_check = _load_script("check_python_packages")
    rust_check = _load_script("check_rust_packages")
    project = next(project for project in python_check.PROJECTS if project.package == "agentctl")
    crate = next(crate for crate in rust_check.CRATES if crate.name == "agentctl")
    comparison = "Python includes worker/Chat/MCP capabilities; Rust provides the interactive core."
    assert not python_check._doc_violations(project, comparison, document_name="USER_GUIDE.md")
    assert not rust_check._doc_violations(crate, comparison, document_name="src/embedded_userguide.md")
    assert python_check._doc_violations(project, comparison)
    assert rust_check._doc_violations(crate, comparison)
    assert python_check._doc_violations(project, comparison, document_name="README.md")
    assert rust_check._doc_violations(crate, comparison, document_name="README.md")
    assert "foreign-language term 'Cargo'" in python_check._doc_violations(
        project, comparison + " Cargo", document_name="USER_GUIDE.md")
    assert "foreign-package term 'pip'" in rust_check._doc_violations(
        crate, comparison + " pip", document_name="src/embedded_userguide.md")


def test_agentctl_protocol_identity_exemption_does_not_allow_sibling_package_prose() -> None:
    rust_check = _load_script("check_rust_packages")
    agentctl = next(crate for crate in rust_check.CRATES if crate.name == "agentctl")

    assert not rust_check._doc_violations(
        agentctl,
        '"capability":"chat-subscription.example" '
        '"protocol":{"name":"agentctl-chat-subscription"} '
        "chat-subscription.google-chat.workspace-events "
        "chat-subscription.google-chat.polling "
        "https://docs.rs/chat-subscription-plugin/0.1.0/chat_subscription_plugin/",
    )
    assert "sibling package 'chat-subscription'" in rust_check._doc_violations(
        agentctl, "Import the chat-subscription package."
    )
    assert "sibling package 'chat-subscription'" in rust_check._doc_violations(
        agentctl, '"capability":"chat-subscription.unreviewed"'
    )
    plugin = next(
        crate for crate in rust_check.CRATES if crate.name == "chat-subscription-plugin"
    )
    assert not rust_check._doc_violations(plugin, "agentctl-chat-subscription")
    assert "sibling package 'chat-subscription'" in rust_check._doc_violations(
        plugin, "Import the chat-subscription package."
    )


def test_rust_package_dependency_patch_uses_the_exact_prior_archive(tmp_path: Path) -> None:
    rust_check = _load_script("check_rust_packages")
    source = tmp_path / "source" / "chat-subscription-0.1.0"
    (source / "src").mkdir(parents=True)
    (source / "Cargo.toml").write_text(
        '[package]\nname="chat-subscription"\nversion="0.1.0"\n',
        encoding="utf-8",
    )
    (source / "src" / "lib.rs").write_text("archive bytes\n", encoding="utf-8")
    archive = tmp_path / "chat-subscription-0.1.0.crate"
    with tarfile.open(archive, mode="w:gz") as package:
        package.add(source, arcname=source.name)
    (source / "src" / "lib.rs").write_text("changed live bytes\n", encoding="utf-8")
    plugin = next(
        crate for crate in rust_check.CRATES if crate.name == "chat-subscription-plugin"
    )

    arguments = rust_check._package_patch_arguments(
        plugin, {"chat-subscription": archive}, tmp_path / "target"
    )

    assert arguments[0] == "--config"
    prefix = "patch.crates-io.chat-subscription.path="
    assert arguments[1].startswith(prefix)
    extracted = Path(json.loads(arguments[1][len(prefix) :]))
    assert (extracted / "src" / "lib.rs").read_text(encoding="utf-8") == "archive bytes\n"
    assert extracted != source


def test_rust_package_creation_shim_is_locked_and_never_verifies_live_source(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    rust_check = _load_script("check_rust_packages")
    plugin = next(
        crate for crate in rust_check.CRATES if crate.name == "chat-subscription-plugin"
    )
    fake_rs_root = tmp_path / "rs"
    (fake_rs_root / plugin.name).mkdir(parents=True)
    (fake_rs_root / "chat-subscription").mkdir()
    monkeypatch.setattr(rust_check, "RS_ROOT", fake_rs_root)
    observed: list[str] = []

    def run(command: list[str]) -> subprocess.CompletedProcess[str]:
        observed.extend(command)
        target = Path(command[command.index("--target-dir") + 1])
        archive = target / "package" / f"{plugin.name}-0.1.0.crate"
        archive.parent.mkdir(parents=True)
        archive.write_bytes(b"fixture archive")
        return subprocess.CompletedProcess(command, 0, "", "")

    monkeypatch.setattr(rust_check, "_run", run)
    rust_check._package(plugin, "0.1.0", tmp_path / "target")

    assert "--no-verify" in observed
    assert "--locked" in observed
    patch = next(value for value in observed if value.startswith("patch.crates-io."))
    assert str(fake_rs_root / "chat-subscription") in patch


def test_rust_archive_lock_subset_binds_versions_but_allows_smaller_feature_graph(
    tmp_path: Path,
) -> None:
    rust_check = _load_script("check_rust_packages")

    def lock(path: Path, version: str, dependencies: str) -> None:
        path.write_text(
            "# This file is automatically @generated by Cargo.\n"
            "# It is not intended for manual editing.\n"
            "version = 4\n\n"
            "[[package]]\n"
            'name = "fixture"\n'
            f'version = "{version}"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
            'checksum = "abc"\n'
            f"dependencies = [{dependencies}]\n",
            encoding="utf-8",
        )

    workspace = tmp_path / "workspace.lock"
    generated = tmp_path / "generated.lock"
    lock(workspace, "1.2.3", '\n "extra",\n')
    lock(generated, "1.2.3", "")
    rust_check._verify_lock_subset(generated, workspace)

    lock(generated, "1.2.4", "")
    with pytest.raises(rust_check.CheckError, match="fixture 1.2.4"):
        rust_check._verify_lock_subset(generated, workspace)


def test_rust_package_dependency_patch_requires_prior_archive_and_rejects_links(
    tmp_path: Path,
) -> None:
    rust_check = _load_script("check_rust_packages")
    plugin = next(
        crate for crate in rust_check.CRATES if crate.name == "chat-subscription-plugin"
    )
    with pytest.raises(rust_check.CheckError, match="was not packaged and inspected first"):
        rust_check._package_patch_arguments(plugin, {}, tmp_path / "target")

    archive = tmp_path / "chat-subscription-0.1.0.crate"
    with tarfile.open(archive, mode="w:gz") as package:
        manifest = tarfile.TarInfo("chat-subscription-0.1.0/Cargo.toml")
        manifest.size = 0
        package.addfile(manifest)
        link = tarfile.TarInfo("chat-subscription-0.1.0/src/lib.rs")
        link.type = tarfile.SYMTYPE
        link.linkname = "/tmp/outside"
        package.addfile(link)
    with pytest.raises(rust_check.CheckError, match="contains non-file"):
        rust_check._extract_dependency_archive(
            "chat-subscription", archive, tmp_path / "extract"
        )


def test_rust_workspace_topology_requires_public_members_and_exempts_only_publish_false(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    rust_check = _load_script("check_rust_packages")
    checked = rust_check.Crate("checked", (), "checked", ())
    monkeypatch.setattr(rust_check, "CRATES", (checked,))

    def metadata(packages: list[dict[str, object]]) -> object:
        return subprocess.CompletedProcess(
            args=[], returncode=0, stdout=json.dumps({"packages": packages}), stderr=""
        )

    monkeypatch.setattr(
        rust_check,
        "_run",
        lambda _argv: metadata(
            [
                {"name": "checked", "publish": None},
                {"name": "fake", "publish": []},
            ]
        ),
    )
    rust_check._check_workspace_topology()

    monkeypatch.setattr(
        rust_check,
        "_run",
        lambda _argv: metadata(
            [
                {"name": "checked", "publish": None},
                {"name": "omitted", "publish": None},
            ]
        ),
    )
    with pytest.raises(rust_check.CheckError, match="omitted.*missing from the package gate"):
        rust_check._check_workspace_topology()


def test_python_sibling_dependency_requires_a_project_local_exemption() -> None:
    python_check = _load_script("check_python_packages")
    parallel = next(
        project
        for project in python_check.PROJECTS
        if project.distribution == "parallel-experiment-runner"
    )
    dagrun_requirement = {"dagrun"}

    assert not python_check._unexpected_sibling_requirements(
        parallel, dagrun_requirement
    )
    assert python_check._unexpected_sibling_requirements(
        python_check.PROJECTS[0], {"tick-hub"}
    ) == ["tick-hub"]


def test_every_declared_markdown_resource_is_standalone() -> None:
    python_check = _load_script("check_python_packages")

    for project in python_check.PROJECTS:
        source = python_check.PY_ROOT / project.directory
        # Reading the declared resources is the artifact checker's common source/sdist/wheel gate.
        python_check._source_resources(project, source)


def test_secondary_markdown_resource_gets_the_standalone_lint(tmp_path: Path) -> None:
    python_check = _load_script("check_python_packages")
    project = python_check.Project(
        directory="demo",
        distribution="demo",
        package="demo",
        commands=("demo",),
        resources=("AGENT_USER_GUIDE.md",),
        required_dependencies=(),
    )
    (tmp_path / "AGENT_USER_GUIDE.md").write_text(
        "Use the DeepScry workspace.\n", encoding="utf-8"
    )

    with pytest.raises(
        python_check.CheckError,
        match=r"AGENT_USER_GUIDE\.md is not standalone: unrelated project 'DeepScry'",
    ):
        python_check._source_resources(project, tmp_path)


def test_unexpected_package_members_reject_undeclared_documentation() -> None:
    python_check = _load_script("check_python_packages")

    unexpected = python_check._unexpected_package_members(
        {
            "demo/__init__.py",
            "demo/README.md",
            "demo/ARCHITECTURE.md",
            "demo/static/",
        },
        "demo/",
        {"demo/__init__.py"},
        ("README.md",),
    )

    assert unexpected == ["demo/ARCHITECTURE.md"]


def test_wheel_rejects_present_but_corrupted_declared_resource(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    python_check = _load_script("check_python_packages")
    repo_root = tmp_path / "repo"
    py_root = repo_root / "py"
    source = py_root / "asset_demo"
    (source / "static").mkdir(parents=True)

    readme = "# Asset demo\n"
    userguide = "# Asset demo user guide\n"
    (repo_root / "LICENSE").write_text("test license\n", encoding="utf-8")
    (source / "__init__.py").write_text('"""Asset demo package."""\n', encoding="utf-8")
    (source / "README.md").write_text(readme, encoding="utf-8")
    (source / "USER_GUIDE.md").write_text(userguide, encoding="utf-8")
    (source / "py.typed").write_bytes(b"")
    trusted_core = b"globalThis.TimelineCore = {trusted: true};\n"
    (source / "static" / "timeline-core.js").write_bytes(trusted_core)

    project = python_check.Project(
        directory="asset_demo",
        distribution="asset-demo",
        package="asset_demo",
        commands=("asset-demo",),
        resources=("README.md", "USER_GUIDE.md", "py.typed", "static/timeline-core.js"),
        required_dependencies=(),
    )
    monkeypatch.setattr(python_check, "REPO_ROOT", repo_root)
    monkeypatch.setattr(python_check, "PY_ROOT", py_root)
    monkeypatch.setattr(python_check, "PROJECTS", (project,))

    wheel = tmp_path / "asset_demo-1.0-py3-none-any.whl"
    dist_info = "asset_demo-1.0.dist-info"
    package_prefix = "asset_demo/"
    with zipfile.ZipFile(wheel, mode="w") as archive:
        archive.writestr(
            f"{dist_info}/METADATA",
            "Metadata-Version: 2.4\n"
            "Name: asset-demo\n"
            "Version: 1.0\n"
            "Requires-Python: >=3.10\n"
            "Description-Content-Type: text/markdown\n"
            f"\n{readme}",
        )
        archive.writestr(
            f"{dist_info}/entry_points.txt",
            "[console_scripts]\nasset-demo = asset_demo.cli:main\n",
        )
        archive.writestr(f"{dist_info}/licenses/LICENSE", b"test license\n")
        archive.writestr(f"{package_prefix}__init__.py", b'"""Asset demo package."""\n')
        archive.writestr(f"{package_prefix}README.md", readme)
        archive.writestr(f"{package_prefix}USER_GUIDE.md", userguide)
        archive.writestr(f"{package_prefix}py.typed", b"")
        # This member's presence satisfied the old checker even though its payload is corrupt.
        archive.writestr(
            f"{package_prefix}static/timeline-core.js",
            b"globalThis.TimelineCore = {trusted: false};\n",
        )

    with zipfile.ZipFile(wheel) as archive:
        assert f"{package_prefix}static/timeline-core.js" in archive.namelist()
    with pytest.raises(
        python_check.CheckError,
        match=r"wheel static/timeline-core\.js differs from its authoritative source",
    ):
        python_check._inspect_wheel(project, wheel)


def test_public_api_docs_are_standalone_without_banning_native_package_terms() -> None:
    python_check = _load_script("check_python_packages")
    rust_check = _load_script("check_rust_packages")

    python_check._check_public_docstrings()
    rust_check._check_public_rustdoc()

    assert not python_check._doc_violations(
        python_check.PROJECTS[0], "Install this Python package from PyPI with pip."
    )
    assert not rust_check._doc_violations(
        rust_check.CRATES[0], "Install this Rust crate with Cargo."
    )


def test_dagrun_rust_dependency_snippets_match_the_published_minor_version() -> None:
    manifest = (REPO_ROOT / "rs" / "dagrun" / "Cargo.toml").read_text(encoding="utf-8")
    matched = re.search(r'^version = "(\d+\.\d+)\.\d+"$', manifest, re.MULTILINE)
    assert matched is not None
    dependency = f'dagrun = "{matched.group(1)}"'
    for name in ("README.md", "USER_GUIDE.md"):
        fragment = (
            REPO_ROOT / "common" / "docs" / "dagrun" / "fragments" / "rust" / name
        ).read_text(encoding="utf-8")
        assert dependency in fragment


def test_package_indexes_name_real_check_commands() -> None:
    python_index = (REPO_ROOT / "py" / "README.md").read_text(encoding="utf-8")
    rust_index = (REPO_ROOT / "rs" / "README.md").read_text(encoding="utf-8")
    makefile = (REPO_ROOT / "Makefile").read_text(encoding="utf-8")

    assert "make check-python-packages" in python_index
    assert "make check-rust-packages" in rust_index
    assert "check-python-packages:" in makefile
    assert "check-rust-packages:" in makefile
    assert "python" not in rust_index.lower()


def test_wrkslots_lifecycle_partitions_are_disjoint_and_complete() -> None:
    all_tests = _collect_lifecycle_tests()
    ordinary = _collect_lifecycle_tests("ordinary_environment")
    mapped = _collect_lifecycle_tests("not ordinary_environment")
    mapped_root = _collect_lifecycle_tests("mapped_root_namespace")

    assert ordinary.isdisjoint(mapped)
    assert ordinary | mapped == all_tests
    # The seven setup-hooks tests of 3bae67a3, 1df79bf9 and db6cdf8b are in the
    # mapped partition. Of the three exited-representative census tests, the
    # two cases of the stubbed one are mapped and the real-process one is
    # ordinary.
    # The retained-handle size test is mapped.
    # Of the eight batch-of-one remove tests, the image-backed one is ordinary
    # and the other seven are mapped.
    # The two image-backed remove-agent-batch use-check tests are ordinary.
    # The three relocate-repository tests (one success case and two refusal
    # cases) are mapped.
    # The two strict-replay stranding tests (a repository relocated twice, and
    # an archived slot's deleted repository) are mapped.
    # The 25 validation-run liveness tests of the wrkslots liveness series
    # (merged at 8648bab6) are mapped.
    # The two clean-caches tests of 96d9f268 (unpacked Cargo package sources and
    # unmarked wildcard matches are kept; a nested Git repository under an
    # unmarked match is still refused) are mapped.
    # The user-systemd per-listing bound regression test of
    # https://github.com/rrnewton/agent-utils/issues/235 is mapped.
    # The 14 frozen-validation-parser cases of 88039882, 7ae34255 and 73cdf9c4
    # are 8 ordinary and 6 mapped, all parametrized cases of tests named above.
    # The 38 result-recorded frozen removal cases (one removal, one
    # classification, one direct refusal, 23 refused gates, ten journal
    # resumptions, and two incomplete-result retentions) are ordinary.
    # The twelve salvage cases of https://github.com/rrnewton/agent-utils/issues/237
    # (two removals of a submodule cloned from its source's module directory,
    # one removal of a submodule cloned from another local mirror, one alias
    # refusal, the lock-free publication check, the archive made before the
    # lock, the rewritten-branch check, and five recorded-remote-digest
    # cases) are mapped.
    # The seven salvage cases of its lock-hold follow-up (the existing archive
    # checked before the lock, submodule facts derived before the lock, a file
    # written and index flags set after that check, one clean published
    # shallow submodule, and two refused shallow submodules) are mapped.
    assert len(all_tests) == 1947
    assert len(ordinary) == 336
    assert len(mapped) == 1611
    assert {
        node.split("::", 1)[1].split("[", 1)[0] for node in ordinary
    } == {
        "test_adopt_refuses_pid_outside_invoking_process_ancestry",
        "test_batch_census_isolated_real_file_alias_mapping_and_socket",
        "test_batch_census_two_selected_trees_cannot_mask_socket_holder",
        "test_batch_census_validation_filesystem_outside_hardlink_mapping",
        "test_batch_maps_filter_preserves_large_inode_strings_and_read_errors",
        "test_bounded_read_only_command_cleans_up_post_spawn_setup_failure",
        "test_bounded_read_only_command_discards_output_before_timeout",
        "test_bounded_read_only_command_kills_descendant_holding_output",
        "test_bounded_read_only_command_refuses_when_killed_child_cannot_be_reaped",
        "test_bounded_lsof_reports_installed_binary_file_and_alias_matches",
        "test_create_binds_owner_running_beside_its_assigned_coordinator",
        "test_current_frozen_result_without_removal_proof_blocks_entry",
        "test_current_incomplete_frozen_binds_checkout_and_gitlink_identity",
        "test_current_incomplete_frozen_binds_initial_facts_to_recursive_snapshot",
        "test_current_incomplete_frozen_rechecks_after_fresh_census",
        "test_current_incomplete_frozen_refuses_untrusted_or_live_evidence",
        "test_current_incomplete_frozen_requires_exact_current_shape",
        "test_current_incomplete_frozen_requires_pristine_recursive_checkout",
        "test_current_incomplete_frozen_retains_evidence_and_allows_stable_source_work",
        "test_current_incomplete_frozen_retention_does_not_authorize_direct_recovery",
        "test_current_incomplete_frozen_with_durable_result_is_still_only_retained",
        "test_direct_current_frozen_recovery_refuses_lost_journal_proof",
        "test_direct_current_frozen_recovery_refuses_tampered_proof",
        "test_direct_current_frozen_recovery_requires_removal_proof",
        "test_direct_frozen_recovery_preserves_proofless_legacy_schemas",
        "test_direct_historical_projection_cannot_use_batch_only_nonblocking_result",
        "test_direct_inode_census_preserves_socket_holder_identity",
        "test_frozen_parser_and_consumer_use_exact_real_projection_shape",
        "test_frozen_parser_authority_accepts_real_nested_linked_worktree",
        "test_frozen_parser_failures_preserve_checkout_without_journal",
        "test_frozen_parser_reads_immutable_initial_snapshot_during_live_restore",
        "test_frozen_parser_refuses_each_module_tampered_before_or_after_inspect",
        "test_frozen_parser_refuses_record_mutation_after_inspect",
        "test_frozen_validate_batch_closes_operation_owned_fds_before_censuses",
        "test_frozen_validate_batch_accepts_exact_validation_removal_proof",
        "test_frozen_validate_batch_fresh_census_does_not_ignore_current_process",
        "test_frozen_validate_detects_pre_exclusion_same_uid_holder_and_rolls_back",
        "test_frozen_validate_excludes_late_same_uid_checkout_entry",
        "test_frozen_validate_sealed_guard_fd_cannot_open_late_payload",
        "test_frozen_validate_checkout_binds_terminal_record_fields",
        "test_frozen_validate_checkout_recovers_each_durable_crash_boundary",
        "test_frozen_validate_recovery_refuses_exclusion_identity_tampering",
        "test_frozen_validate_recovery_refuses_absent_fenced_path",
        "test_frozen_validate_recovery_refuses_absent_prepared_path_twice",
        "test_frozen_validate_recovery_refuses_changed_identity_binding",
        "test_frozen_validate_recovery_refuses_changed_terminal_record_digest",
        "test_frozen_validate_rebinds_external_proof_after_exclusion_census",
        "test_frozen_validate_refuses_disappearance_after_final_check",
        "test_frozen_validate_refuses_cross_device_exclusion_root",
        "test_frozen_validate_refuses_fenced_replacement_after_final_check",
        "test_frozen_validate_refuses_replacement_after_final_check",
        "test_frozen_validate_refuses_when_verified_root_context_is_unavailable",
        "test_frozen_validate_rejects_counterfeit_minimal_terminal_records",
        "test_historical_frozen_entry_classification_closes_operation_owned_fds",
        "test_historical_frozen_entry_classification_fails_closed_on_evidence_change",
        "test_historical_frozen_entry_classification_fails_closed_on_shape_changes",
        "test_historical_frozen_live_process_identity_blocks_entry",
        "test_historical_frozen_retention_reports_schema_two_exactly",
        "test_host_context_recover_finish_preserves_actor_and_issued_proof",
        "test_host_context_recover_rechecks_authority_before_locked_writes",
        "test_host_context_recover_rejects_invalid_handoff_before_mutation",
        "test_lock_conflict_refuses_without_state_change",
        "test_lock_conflict_under_json_format_prints_a_json_refusal",
        "test_ownerless_validate_batch_removes_terminal_frozen_checkout",
        "test_process_entering_after_final_scan_before_path_move_is_not_deleted",
        "test_process_path_census_reports_a_live_user_while_unrelated_processes_exit_during_it",
        "test_remove_agent_batch_checks_an_image_backed_slot_with_lsof_before_its_fence",
        "test_remove_agent_batch_refuses_before_fencing_when_the_image_check_outlasts_the_budget",
        "test_remove_agent_batch_removes_disk_image_slots",
        "test_remove_checks_an_image_backed_agent_slot_under_the_lock",
        "test_remove_refuses_live_process_using_slot",
        "test_result_recorded_frozen_checkout_classifies_as_terminal_and_stays",
        "test_result_recorded_frozen_checkout_is_removed_by_batch",
        "test_result_recorded_frozen_checkout_still_refuses_direct_recovery",
        "test_result_recorded_frozen_removal_refuses_each_unproven_gate",
        "test_result_recorded_frozen_removal_resumes_from_its_journal",
        "test_root_owned_executable_accepts_host_root_helper",
        "test_run1773_historical_frozen_checkout_is_retained_without_blocking_entry",
        "test_socket_orphan_namespace_unavailable_is_an_exact_classifier",
        "test_uncontained_current_frozen_checkout_still_cannot_be_removed",
        "test_uncontained_historical_frozen_retention_binds_head_and_source",
        "test_validate_batch_rechecks_external_proof_after_private_seal",
        "test_validate_complete_still_refuses_live_path_use_after_owner_dies",
    }

    negative = (
        "wrkslots/tests/test_lifecycle.py::"
        "test_root_owned_executable_rejects_namespace_root_without_host_root"
    )
    exclusion_root = (
        "wrkslots/tests/test_lifecycle.py::"
        "test_validation_exclusion_fixture_preserves_host_root_boundary"
    )
    assert mapped_root == {negative, exclusion_root}
    assert negative in mapped
    assert negative not in ordinary
    assert exclusion_root in mapped
    assert exclusion_root not in ordinary

    replay_test = (
        "wrkslots/tests/test_lifecycle.py::"
        "test_doctor_replays_hold_events_once_per_machine"
    )
    assert replay_test in mapped
    assert replay_test not in ordinary
    assert replay_test not in mapped_root

    degenerate_owner_test = (
        "wrkslots/tests/test_lifecycle.py::"
        "test_remove_releases_a_slot_whose_owner_record_is_degenerate"
    )
    assert degenerate_owner_test in mapped
    assert degenerate_owner_test not in ordinary
    assert degenerate_owner_test not in mapped_root

    makefile = re.sub(
        r"\s+",
        " ",
        (REPO_ROOT / "Makefile").read_text(encoding="utf-8").replace("\\\n", " "),
    )
    assert (
        'test_python="$$(python3 -c '
        "'import os, sys; print(os.path.realpath(sys.executable))')\" && "
        "unshare --user --map-root-user --pid --fork --mount-proc "
        '"$$test_python" ../scripts/pid_namespace_init.py -- '
        "python3 -m pytest -q -c pyproject.toml --rootdir=. "
        "wrkslots/tests/test_lifecycle.py -m 'not ordinary_environment'"
    ) in makefile
    assert (
        "python3 -m pytest -q -c pyproject.toml --rootdir=. "
        "wrkslots/tests/test_lifecycle.py -m ordinary_environment"
    ) in makefile
    assert "wrkslots/tests/test_lifecycle.py::" not in makefile

"""Tests for the disk-image slot representation and the slot sandbox.

Pure tests always run. Tests that need a real mount (``sudo -n`` or FUSE) or a
systemd user manager skip with the reason when the host cannot provide it; the
end-to-end lifecycle exercise is ``wrkslots/tests/e2e_images.sh``.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import re
import shutil
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

from wrkslots import cli, imagecmd, sandbox, slotimage


def _init_args(**overrides: object) -> argparse.Namespace:
    values: dict[str, object] = {
        "slot_representation": None,
        "image_ceiling_gib": None,
        "image_backend": None,
    }
    values.update(overrides)
    return argparse.Namespace(**values)


def test_new_project_defaults_to_images(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("WRKSLOTS_INIT_REPRESENTATION", raising=False)
    representation, image = cli._init_representation(_init_args(), tmp_path / ".wrkslots.yml")
    assert representation == "image"
    assert image is None


def test_existing_project_keeps_worktrees_when_key_absent(tmp_path: Path) -> None:
    config = tmp_path / ".wrkslots.yml"
    config.write_text(json.dumps({"schema": 2}), encoding="utf-8")
    representation, _image = cli._init_representation(_init_args(), config)
    assert representation == "worktree"


def test_explicit_flag_wins_and_image_options_merge(tmp_path: Path) -> None:
    config = tmp_path / ".wrkslots.yml"
    config.write_text(json.dumps({"image": {"backend": "fuse"}}), encoding="utf-8")
    representation, image = cli._init_representation(
        _init_args(slot_representation="image", image_ceiling_gib=7), config
    )
    assert representation == "image"
    assert image == {"backend": "fuse", "ceiling_bytes": 7 * cli.GIB}


def test_worktree_representation_is_omitted_from_payload() -> None:
    payload = cli._config_payload("worktrees/slots", "m", "origin", "refs/remotes/origin/main", 60, "t.py")
    assert "slot_representation" not in payload
    image_payload = cli._config_payload(
        "worktrees/slots", "m", "origin", "refs/remotes/origin/main", 60, "t.py",
        slot_representation="image",
    )
    assert image_payload["slot_representation"] == "image"
    assert cli._canonical_config_payload({**payload, "slot_representation": "worktree"}) == payload


def test_image_settings_validation() -> None:
    assert cli._parse_image_settings({}) == slotimage.ImageSettings()
    parsed = cli._parse_image_settings({"image": {"ceiling_bytes": 2 * cli.GIB, "backend": "fuse"}})
    assert parsed.ceiling_bytes == 2 * cli.GIB and parsed.backend == "fuse"
    with pytest.raises(cli.StateError):
        cli._parse_image_settings({"image": {"backend": "loopback"}})
    with pytest.raises(cli.StateError):
        cli._parse_image_settings({"image": {"surprise": 1}})
    with pytest.raises(cli.StateError):
        cli._parse_slot_representation({"slot_representation": "zfs"})


def test_sandbox_settings_validation() -> None:
    settings = sandbox.settings_from_obj(
        {
            "isolation": "root",
            "home": "hidden",
            "home_expose": ["bin"],
            "read_write": ["/var/x/$USER", "~/scratch"],
            "env": {"A_B": "~/x"},
            "limits": {"memory_max": "32G"},
        }
    )
    assert settings.isolation == "root" and settings.home == "hidden"
    assert settings.home_expose == ("bin",) and settings.limits.memory_max == "32G"
    assert settings.limits.tasks_max == sandbox.DEFAULT_TASKS_MAX
    for bad, fragment in (
        ({"home": "rw"}, "home must be"),
        ({"home": "none"}, "hidden with home_expose"),
        ({"isolation": "namespace"}, "renamed userns"),
        ({"home_expose": ["../escape"]}, "without '..'"),
        ({"home_shared": ["/abs"]}, "relative"),
        ({"home_private_files": [".config/x"]}, "top-level"),
        ({"outputs": ["."]}, "relative"),
        ({"read_write": ["relative/path"]}, "absolute"),
        ({"env": {"1BAD": "x"}}, "invalid variable name"),
        ({"env": ["A=1"]}, "object"),
        ({"tmp_size": "lots"}, "tmp_size"),
        ({"limits": {"memory_max": "lots"}}, "32G"),
        ({"limits": {"tasks_max": 0}}, "positive"),
        ({"limits": {"surprise": 1}}, "unknown keys: surprise"),
        ({"memory_max": "32G"}, "now limits.memory_max"),
        ({"home_writable": [".cache"]}, "now home_private"),
        ({"unknown": 1}, "unknown keys: unknown"),
    ):
        with pytest.raises(sandbox.SandboxError, match=re.escape(fragment)):
            sandbox.settings_from_obj(bad)


def test_default_sandbox_section_spells_out_every_key() -> None:
    section = sandbox.default_config_obj()
    assert list(section) == list(sandbox.SETTING_KEYS)
    assert section["limits"] == {key: getattr(sandbox.SandboxLimits(), key) for key in sandbox.LIMIT_KEYS}
    assert sandbox.settings_from_obj(section) == sandbox.SandboxSettings()
    assert sandbox.settings_from_obj({}) == sandbox.SandboxSettings()
    assert section["isolation"] == "userns" and section["home"] == "ro"
    shared = section["home_shared"]
    assert isinstance(shared, list) and ".local/share/muse" in shared
    assert section["home_private_files"] == [".claude.json"]
    # Package caches are shared; the rest of ~/.cargo (bin, config.toml) stays read-only.
    assert ".cargo/registry" in shared and ".cargo/git" in shared
    assert ".cargo" not in shared and ".cargo/bin" not in shared
    assert section["outputs"] == ["ai_docs", "experiments"]
    assert section["env"] == {} and section["tmp_size"] == "16G"
    json.dumps(section)


def test_merge_defaults_adds_only_missing_keys() -> None:
    existing: dict[str, object] = {"home": "hidden", "env": {"A": "1"}, "limits": {"memory_max": "8G"}}
    merged, added = sandbox.merge_defaults(existing)
    assert merged["home"] == "hidden" and merged["env"] == {"A": "1"}
    default_limits = sandbox.default_config_obj()["limits"]
    assert isinstance(default_limits, dict)
    assert merged["limits"] == {**default_limits, "memory_max": "8G"}
    assert "home" not in added and "env" not in added and "limits.memory_max" not in added
    assert "isolation" in added and "limits.cpu_quota" in added
    again, added_again = sandbox.merge_defaults(merged)
    assert again == merged and added_again == []
    with pytest.raises(sandbox.SandboxError):
        sandbox.merge_defaults({"legacy": True})


def test_init_writes_the_full_sandbox_section_only_for_new_projects(tmp_path: Path) -> None:
    config = tmp_path / ".wrkslots.yml"
    assert cli._init_sandbox_section(config) == sandbox.default_config_obj()
    config.write_text(json.dumps({"schema": 2}), encoding="utf-8")
    assert cli._init_sandbox_section(config) is None
    config.write_text(json.dumps({"sandbox": {"home": "hidden"}}), encoding="utf-8")
    assert cli._init_sandbox_section(config) == {"home": "hidden"}
    payload = cli._config_payload(
        "worktrees/slots", "m", "origin", "refs/remotes/origin/main", 60, "t.py",
        sandbox_section=sandbox.default_config_obj(),
    )
    assert payload["sandbox"] == sandbox.default_config_obj()



def test_init_sandbox_isolation_sets_the_new_projects_box(tmp_path: Path) -> None:
    project = tmp_path / "project"
    project.mkdir()
    liveness = project / "liveness.py"
    liveness.write_text("#!/bin/sh\nexit 2\n", encoding="utf-8")
    liveness.chmod(0o755)
    environment = {
        **os.environ,
        "PYTHONPATH": str(Path(__file__).resolve().parents[2]),
        "WRKSLOTS_INIT_REPRESENTATION": "worktree",
    }

    def init(*extra: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-m", "wrkslots", "--machine", "testhost", "init", str(project),
             "--liveness-command", "liveness.py", *extra],
            cwd=project, capture_output=True, text=True, timeout=120, env=environment, check=False,
        )

    first = init("--sandbox-isolation", "cgroup")
    assert first.returncode == 0, first.stderr
    section = cli._read_config(project / ".wrkslots.yml")["sandbox"]
    assert isinstance(section, dict)
    assert section == {**sandbox.default_config_obj(), "isolation": "cgroup"}
    assert init("--sandbox-isolation", "cgroup").returncode == 0
    changed = init("--sandbox-isolation", "root")
    assert changed.returncode != 0
    assert "already has different configuration" in changed.stderr

def test_expand_path_knows_only_home_and_user() -> None:
    home, user = "/h/u", "u"
    assert sandbox.expand_path("~", home, user) == "/h/u"
    assert sandbox.expand_path("~/x", home, user) == "/h/u/x"
    assert sandbox.expand_path("/var/c/$USER/x", home, user) == "/var/c/u/x"
    assert sandbox.expand_path("/var/c/${USER}/x", home, user) == "/var/c/u/x"
    assert sandbox.expand_path("$HOME/y", home, user) == "/h/u/y"
    assert sandbox.expand_path("/var/$USERNAME", home, user) == "/var/$USERNAME"
    assert sandbox.expand_path("/a/~b", home, user) == "/a/~b"


def _fake_home(base: Path) -> Path:
    home = base / "home"
    for directory in (".config/gh", ".config/muse", ".local/bin", ".ssh", ".cache", "bin", "work"):
        (home / directory).mkdir(parents=True)
    (home / ".config/gh/hosts.yml").write_text("token: secret\n", encoding="utf-8")
    (home / ".config/gh/config.yml").write_text("editor: vi\n", encoding="utf-8")
    (home / ".local/bin/tool").write_text("#!/bin/sh\necho tool\n", encoding="utf-8")
    (home / ".local/bin/tool").chmod(0o755)
    (home / ".ssh/id_x").write_text("private\n", encoding="utf-8")
    (home / ".netrc").write_text("machine x password y\n", encoding="utf-8")
    (home / ".claude.json").write_text('{"seeded": true}\n', encoding="utf-8")
    (home / ".profile").write_text("# profile\n", encoding="utf-8")
    (home / "link").symlink_to("work")
    return home


def _view(base: Path) -> sandbox.SlotView:
    return sandbox.SlotView(
        slot="s1",
        slot_type="agent",
        slot_path=base / "slot",
        state_directory=base / "state",
        git_directories=(base / "repo.git",),
        control_directory=base / "control",
        representation="worktree",
        project_root=base / "project",
    )


def test_build_spec_layers_home_and_binds_blessed_paths(tmp_path: Path) -> None:
    home = _fake_home(tmp_path)
    for directory in ("slot", "repo.git", "control", "project/ai_docs"):
        (tmp_path / directory).mkdir(parents=True)
    view = _view(tmp_path)
    spec = sandbox.build_spec(view, sandbox.SandboxSettings(), home, tmp_path / "slot")
    binds = spec["binds"]
    assert isinstance(binds, list)
    targets = [target for _source, target in binds]
    # Shared paths are bound onto themselves, nested ones included; missing ones are skipped.
    assert [str(home / ".config/muse")] * 2 in binds
    assert str(home / ".claude") not in targets
    for path in ("slot", "repo.git", "project/ai_docs"):
        assert str(tmp_path / path) in targets
    # The control directory is never writable from a box; other slots' state is masked.
    assert str(tmp_path / "control") not in targets
    assert spec["read_only"][0] == str(tmp_path / "control")  # type: ignore[index]
    assert str(tmp_path / "control" / "slot-state") in spec["masks"]  # type: ignore[operator]
    assert str(tmp_path / "control" / "slot-images") in spec["masks"]  # type: ignore[operator]
    assert str(tmp_path / "project/experiments") not in targets  # missing output: skipped
    # Every real top-level entry is bound read-only over the layer, .config and
    # .local included; private directories and private files are not.
    home_binds = spec["home_binds"]
    assert isinstance(home_binds, list)
    assert {".config", ".local", ".ssh", "bin", "work", ".netrc", ".profile"} <= set(home_binds)
    assert ".cache" not in home_binds and ".claude.json" not in home_binds and "link" not in home_binds
    masks = spec["masks"]
    assert isinstance(masks, list)
    assert str(home / ".ssh") in masks and str(home / ".config/gh/hosts.yml") in masks
    assert spec["home_layer"] == str(tmp_path / "state" / "home")
    hidden = sandbox.build_spec(view, sandbox.SandboxSettings(home="hidden"), home, tmp_path / "slot")
    assert hidden["home_binds"] == ["bin", ".local/bin"]


def test_prepare_state_builds_the_layer_without_touching_home(tmp_path: Path) -> None:
    home = _fake_home(tmp_path)
    before = sorted(str(path.relative_to(home)) for path in home.rglob("*"))
    view = _view(tmp_path)
    sandbox.prepare_state(view, sandbox.SandboxSettings(), home)
    layer = tmp_path / "state" / "home"
    assert (layer / ".config").is_dir() and not any((layer / ".config").iterdir())
    assert (layer / ".profile").is_file() and (layer / ".profile").stat().st_size == 0
    assert os.readlink(layer / "link") == "work"
    assert json.loads((layer / ".claude.json").read_text(encoding="utf-8")) == {"seeded": True}
    assert (layer / ".cache").is_dir() and (layer / ".buck").is_dir()
    (layer / ".claude.json").write_text("{}", encoding="utf-8")
    sandbox.prepare_state(view, sandbox.SandboxSettings(), home)
    assert (layer / ".claude.json").read_text(encoding="utf-8") == "{}"  # seeded once
    # Switching to hidden removes the ro placeholders and links it created,
    # but never content the agent wrote.
    (layer / ".profile").write_text("agent content", encoding="utf-8")
    sandbox.prepare_state(view, sandbox.SandboxSettings(home="hidden"), home)
    assert not (layer / ".config" / "gh").exists() and (layer / ".config" / "muse").is_dir()
    assert not os.path.lexists(layer / "link") and not (layer / "work").exists()
    assert (layer / ".profile").read_text(encoding="utf-8") == "agent content"
    assert (layer / "bin").is_dir() and (layer / ".local" / "bin").is_dir()
    assert sorted(str(path.relative_to(home)) for path in home.rglob("*")) == before


def test_child_environment(tmp_path: Path) -> None:
    view = _view(tmp_path)
    environ = {"HOME": "/h/u", "PATH": "/bin", "TMPDIR": "/var/tmp/x", "INVOCATION_ID": "1", "TMP": "/t"}
    settings = sandbox.SandboxSettings(env=(("CONF", "~/.conf"),))
    boxed = sandbox.child_environment(view, settings, environ)
    assert boxed["TMPDIR"] == "/tmp" and "TMP" not in boxed and "INVOCATION_ID" not in boxed
    assert boxed["CONF"] == "/h/u/.conf" and boxed["WRKSLOTS_SANDBOX"] == "userns"
    assert boxed["WRKSLOTS_SLOT_REPRESENTATION"] == "worktree"
    limited = sandbox.child_environment(view, dataclasses.replace(settings, isolation="cgroup"), environ)
    assert limited["TMPDIR"] == "/var/tmp/x" and limited["TMP"] == "/t"


def test_slot_slices_nest_under_the_callers_slice(tmp_path: Path) -> None:
    cgroup = tmp_path / "cgroup"
    cgroup.write_text(
        "0::/user.slice/user-1.slice/user@1.service/jail.slice/agent-7.scope\n", encoding="utf-8"
    )
    assert sandbox.parent_slice(cgroup) == "jail.slice"
    assert sandbox.root_slice("jail.slice") == "jail-wrkslots.slice"
    if shutil.which("systemd-escape") is None:
        pytest.skip("systemd-escape is not installed")
    assert sandbox.slice_name("agent", "kvm-foo", "jail.slice") == "jail-wrkslots-kvm\\x2dfoo.slice"
    cgroup.write_text("0::/user.slice/user-1.slice/user@1.service/app.scope\n", encoding="utf-8")
    assert sandbox.parent_slice(cgroup) is None
    assert sandbox.slice_name("validate", "v1", "x.slice").startswith("x-wrkslots-validate")


def test_owned_mount_lines_are_excluded_from_use_evidence(monkeypatch: pytest.MonkeyPatch) -> None:
    line = "1 2 0:1 / /p/worktrees/slots/s1 rw - fuse.ext4 /p/slot.img rw"
    other = "3 2 0:2 / /p/worktrees/slots/s1/bind rw - ext4 /dev/sda1 rw"
    monkeypatch.setattr(
        cli, "_OWNED_REPRESENTATION_MOUNTS", frozenset({(Path("/p/worktrees/slots/s1"), "/p/slot.img")})
    )
    parsed = cli._parse_mountinfo_paths(f"{line}\n{other}\n", "test")
    assert [str(path) for path, _raw in parsed] == ["/", "/p/worktrees/slots/s1/bind", "/dev/sda1"]
    assert cli._is_owned_image_root(Path("/p/worktrees/slots/s1"))
    assert not cli._is_owned_image_root(Path("/p/worktrees/slots/s2"))


def _backend_or_skip() -> str:
    try:
        return slotimage.resolve_backend("auto")
    except slotimage.ImageError as exc:
        pytest.skip(f"no slot image backend on this host: {exc}")


def test_image_lifecycle_relocate_and_destroy(tmp_path: Path) -> None:
    backend = _backend_or_skip()
    control = tmp_path / "control"
    (control / "slots").mkdir(parents=True)
    location = control / "slots" / "s1"
    slotimage.register_layout(control, lambda _slot_type, slot: control / "slots" / slot)
    image = slotimage.provision(
        control, "agent", "s1", location, slotimage.ImageSettings(ceiling_bytes=2 * cli.GIB, backend=backend)
    )
    try:
        assert slotimage.is_mounted(image.slot_image, location)
        assert os.listdir(location) == []
        assert sorted(os.listdir(image.state_mount)) == ["home"]
        # Sparse: the image costs far less than its ceiling.
        assert os.stat(image.slot_image).st_blocks * 512 < 256 * 1024 * 1024
        (location / "kept").write_text("x", encoding="utf-8")
        (location / "deleted").write_text("y", encoding="utf-8")
        (location / "deleted").unlink()
        with pytest.raises(slotimage.ImageError):
            slotimage.destroy(image)
        moved = slotimage.relocate(image, control / "slots" / ".s1.fenced")
        assert not location.exists()
        # A file deleted just before the move must not reappear afterwards.
        assert os.listdir(moved.location) == ["kept"]
        assert slotimage.image_for_location(control, moved.location) == moved
        (moved.location / "kept").unlink()
        slotimage.destroy(moved)
        assert not moved.location.exists() and not moved.directory.exists()
    finally:
        for leftover in slotimage.all_images(control):
            slotimage.destroy(leftover, allow_content=True)


def _sandbox_host_or_skip(isolation: str) -> None:
    if shutil.which("systemd-run") is None or shutil.which("busctl") is None:
        pytest.skip("needs a systemd user manager")
    if subprocess.run(["systemctl", "--user", "show-environment"], capture_output=True, check=False).returncode:
        pytest.skip("the systemd user manager is not reachable")
    if isolation == "userns":
        if subprocess.run(["unshare", "-rm", "true"], capture_output=True, check=False).returncode:
            pytest.skip("unprivileged user and mount namespaces are unavailable")
    elif isolation == "root":
        if os.getuid() == 0 or shutil.which("sudo") is None:
            pytest.skip("isolation root needs a non-root user with sudo")
        if subprocess.run(["sudo", "-n", "true"], capture_output=True, check=False).returncode:
            pytest.skip("isolation root needs passwordless sudo")


@pytest.fixture
def box_base() -> Iterator[Path]:
    """A scratch directory OUTSIDE /tmp: the box replaces /tmp with a fresh tmpfs."""

    import tempfile

    root = Path("/var/tmp")
    if not root.is_dir() or not os.access(root, os.W_OK):
        pytest.skip("/var/tmp is not writable")
    base = Path(tempfile.mkdtemp(prefix="wrkslots-box-", dir=root))
    try:
        yield base
    finally:
        subprocess.run(["chmod", "-R", "u+w", str(base)], capture_output=True, check=False)
        shutil.rmtree(base, ignore_errors=True)


_PROBE = r"""
report() { if eval "$2" >/dev/null 2>&1; then echo "$1=yes"; else echo "$1=no"; fi; }
report tmpdir '[ "$TMPDIR" = /tmp ]'
report tmp_fresh '[ -z "$(ls -A /tmp)" ]'
report tmp_write 'echo x > /tmp/t'
report slot_write 'echo x > "$WRKSLOTS_SLOT_PATH/in"'
report output_write 'echo x > "$BASE/project/ai_docs/note"'
report project_write 'echo x > "$BASE/project/src/no"'
report outside_write 'echo x > "$BASE/outside/no"'
report home_new_file 'echo x > ~/new-top-level'
report home_work_write 'echo x > ~/work/no'
report config_read 'grep -q editor ~/.config/gh/config.yml'
report config_write 'echo x > ~/.config/no'
report shared_write 'echo x > ~/.config/muse/shared'
report local_bin_runs '[ "$(~/.local/bin/tool)" = tool ]'
report ssh_empty '[ -z "$(ls -A ~/.ssh)" ]'
report netrc_empty '[ ! -s ~/.netrc ]'
report gh_hosts_empty '[ ! -s ~/.config/gh/hosts.yml ]'
report cache_write 'echo x > ~/.cache/private'
report rename_top_level 'echo "{}" > ~/.claude.json.tmp.1 && mv ~/.claude.json.tmp.1 ~/.claude.json'
"""


def _probe_results(stdout: str) -> dict[str, bool]:
    results: dict[str, bool] = {}
    for line in stdout.splitlines():
        key, separator, value = line.partition("=")
        if separator and value in ("yes", "no"):
            results[key] = value == "yes"
    return results


_EXPECTED = {
    "tmpdir": True,
    "tmp_fresh": True,
    "tmp_write": True,
    "slot_write": True,
    "output_write": True,
    "project_write": False,
    "outside_write": False,
    "home_new_file": True,
    "home_work_write": False,
    "config_read": True,
    "config_write": False,
    "shared_write": True,
    "local_bin_runs": True,
    "ssh_empty": True,
    "netrc_empty": True,
    "gh_hosts_empty": True,
    "cache_write": True,
    "rename_top_level": True,
}


def _assert_home_untouched_and_layered(base: Path, home: Path, state: Path) -> None:
    assert (home / ".config/muse/shared").exists()  # shared: the real file
    assert not (home / "new-top-level").exists() and (state / "home/new-top-level").exists()
    assert not (home / ".cache/private").exists() and (state / "home/.cache/private").exists()
    assert json.loads((home / ".claude.json").read_text(encoding="utf-8")) == {"seeded": True}
    assert (state / "home/.claude.json").read_text(encoding="utf-8").strip() == "{}"
    assert not (state / "tmp").exists()  # /tmp is per launch
    assert not (base / "outside/no").exists() and not (base / "project/src/no").exists()


@pytest.mark.parametrize("isolation", ["userns", "root"])
def test_sandbox_view_confines_writes(box_base: Path, isolation: str) -> None:
    _sandbox_host_or_skip(isolation)
    home = _fake_home(box_base)
    for directory in ("slot", "state", "control", "outside", "project/ai_docs", "project/src"):
        (box_base / directory).mkdir(parents=True)
    slot = f"pytest-{isolation}-{os.getpid()}"
    script = (
        "import sys\n"
        "from pathlib import Path\n"
        "from wrkslots import sandbox\n"
        "base = Path(sys.argv[1])\n"
        f"view = sandbox.SlotView({slot!r}, 'agent', base / 'slot', base / 'state', (), base / 'control', 'worktree', base / 'project')\n"
        f"settings = sandbox.SandboxSettings(isolation={isolation!r}, tmp_size='64M', limits=sandbox.SandboxLimits(tasks_max=256))\n"
        "sandbox.run(view, settings, sys.argv[2:])\n"
    )
    environment = {
        **os.environ,
        "HOME": str(home),
        "BASE": str(box_base),
        "PYTHONPATH": str(Path(__file__).resolve().parents[2]),
    }
    try:
        result = subprocess.run(
            [sys.executable, "-c", script, str(box_base), "bash", "-c", _PROBE],
            capture_output=True,
            text=True,
            timeout=120,
            env=environment,
            check=False,
        )
    finally:
        subprocess.run(["systemctl", "--user", "stop", sandbox.slice_name("agent", slot)], capture_output=True, check=False)
    assert result.returncode == 0, result.stderr
    assert _probe_results(result.stdout) == _EXPECTED, result.stdout + result.stderr
    _assert_home_untouched_and_layered(box_base, home, box_base / "state")


def test_wrkslots_run_boxes_a_plain_worktree_slot(box_base: Path) -> None:
    """The box does not depend on the representation: a plain-worktree slot gets the same view."""

    _sandbox_host_or_skip("userns")
    home = _fake_home(box_base)
    (box_base / "outside").mkdir()
    remote, project = box_base / "remote.git", box_base / "project"
    repository = project / "src"
    git_env = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "HOME": str(home)}

    def git(*arguments: str, cwd: Path | None = None) -> None:
        subprocess.run(["git", *arguments], cwd=cwd, check=True, capture_output=True, text=True, env=git_env)

    git("init", "--bare", "--initial-branch=main", str(remote))
    project.mkdir()
    git("clone", str(remote), str(repository))
    for key, value in (("user.name", "t"), ("user.email", "t@example.invalid")):
        git("config", key, value, cwd=repository)
    (repository / "seed").write_text("seed\n", encoding="utf-8")
    git("add", "seed", cwd=repository)
    git("commit", "-m", "seed", cwd=repository)
    git("push", "-u", "origin", "main", cwd=repository)
    (project / "ai_docs").mkdir()
    liveness = project / "liveness.py"
    liveness.write_text("#!/usr/bin/env python3\nraise SystemExit(1)\n", encoding="utf-8")
    liveness.chmod(0o755)
    environment = {
        **git_env,
        "BASE": str(box_base),
        "PYTHONPATH": str(Path(__file__).resolve().parents[2]),
        "WRKSLOTS_INIT_REPRESENTATION": "worktree",
    }

    def wrkslots(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-m", "wrkslots", "--machine", "testhost", *arguments],
            cwd=project,
            capture_output=True,
            text=True,
            timeout=300,
            env=environment,
            check=False,
        )

    initialized = wrkslots("init", str(project), "--worktrees-dir", "worktrees", "--liveness-command", "liveness.py")
    assert initialized.returncode == 0, initialized.stderr
    configuration = cli._read_config(project / ".wrkslots.yml")
    assert configuration["sandbox"] == sandbox.default_config_obj()
    slot = f"box{os.getpid()}"
    created = wrkslots(
        "create", slot, "--slot-type", "agent", "--coordinator-authorized", "--agent", "a1", "--task", "t1", "--purpose", "box test",
        "--owner-pid", str(os.getpid()), "--coordinator-pid", str(os.getpid()),
        "--repo", "src=src", "--branch", "src=agent/box",
    )
    assert created.returncode == 0, created.stderr
    shell = wrkslots("shell-command", slot, "--format", "json")
    assert shell.returncode == 0, shell.stderr
    document = json.loads(shell.stdout)
    assert document["isolation"] == "userns" and document["command"].endswith(" -i")
    harness = wrkslots("shell-command", slot, "--isolation", "root", "--format", "json", "--", "/bin/echo", "a b")
    assert harness.returncode == 0, harness.stderr
    document = json.loads(harness.stdout)
    assert document["isolation"] == "root"
    assert document["command"].endswith("--isolation root -- /bin/echo 'a b'")
    probe = _PROBE + r"""
report representation '[ "$WRKSLOTS_SLOT_REPRESENTATION" = worktree ]'
report checkout_commit 'cd "$WRKSLOTS_SLOT_PATH"/src && echo w > w && git add w && git -c user.name=t -c user.email=t@e commit -qm w'
"""
    try:
        boxed = wrkslots("run", slot, "--", "bash", "-c", probe.replace("$BASE/project/src/no", "$BASE/project/src/seed"))
    finally:
        subprocess.run(["systemctl", "--user", "stop", sandbox.slice_name("agent", slot)], capture_output=True, check=False)
    assert boxed.returncode == 0, boxed.stderr
    expected = {**_EXPECTED, "representation": True, "checkout_commit": True}
    assert _probe_results(boxed.stdout) == expected, boxed.stdout + boxed.stderr
    assert (repository / "seed").read_text(encoding="utf-8") == "seed\n"  # primary checkout unchanged
    # The box's private state beside the slots is storage, not an unregistered slot.
    status = wrkslots("status")
    assert "directory-without-row" not in status.stdout + status.stderr, status.stdout
    state = project / "worktrees" / ".wrkslots" / "slot-state" / "agent" / slot
    if not state.exists():
        candidates = list(project.rglob(f"slot-state/agent/{slot}"))
        assert candidates, "worktree slot state directory not found"
        state = candidates[0]
    _assert_home_untouched_and_layered(box_base, home, state)


_CONTROL_PROBE = r"""
report() { if eval "$2" >/dev/null 2>&1; then echo "$1=yes"; else echo "$1=no"; fi; }
report own_slot_write 'echo x > "$WRKSLOTS_SLOT_PATH/mine"'
report other_slot_write 'echo x > "$OTHER/secret.txt"'
report other_slot_read 'grep -q "other slot" "$OTHER/secret.txt"'
report registry_write 'echo "{}" > "$CONTROL/ACTIVE.testhost.json"'
report registry_new_file 'echo x > "$CONTROL/stray"'
report registry_read 'grep -q registry "$CONTROL/ACTIVE.testhost.json"'
report other_layer_hidden '[ ! -e "$CONTROL/slot-state/agent/other/home/.claude.json" ]'
report images_hidden '[ -z "$(ls -A "$CONTROL/slot-images")" ]'
report home_new_file 'echo x > ~/new-top-level'
"""


@pytest.mark.parametrize("isolation", ["userns", "root"])
@pytest.mark.parametrize("layout", ["nested", "flat"])
@pytest.mark.parametrize("protect_system", [True, False])
def test_other_slots_and_the_registry_are_read_only_from_a_box(
    box_base: Path, isolation: str, layout: str, protect_system: bool
) -> None:
    _sandbox_host_or_skip(isolation)
    home = _fake_home(box_base)
    control = box_base / "wt"
    slots = control if layout == "nested" else control / "slots"
    mine, other = slots / "mine", slots / "other"
    for directory in (mine, other, box_base / "project"):
        directory.mkdir(parents=True)
    (other / "secret.txt").write_text("other slot's work\n", encoding="utf-8")
    (control / "ACTIVE.testhost.json").write_text('{"registry": true}\n', encoding="utf-8")
    other_layer = control / "slot-state" / "agent" / "other" / "home"
    other_layer.mkdir(parents=True)
    (other_layer / ".claude.json").write_text("{}", encoding="utf-8")
    (control / "slot-images" / "agent" / "other").mkdir(parents=True)
    (control / "slot-images" / "agent" / "other" / "IMAGE.json").write_text("{}", encoding="utf-8")
    state = control / "slot-state" / "agent" / "mine"
    slot = f"pytest-ctl-{isolation}-{layout}-{int(protect_system)}-{os.getpid()}"
    script = (
        "import sys\n"
        "from pathlib import Path\n"
        "from wrkslots import sandbox\n"
        "base = Path(sys.argv[1])\n"
        f"view = sandbox.SlotView({slot!r}, 'agent', Path({str(mine)!r}), Path({str(state)!r}), (), "
        f"Path({str(control)!r}), 'worktree', base / 'project')\n"
        f"settings = sandbox.SandboxSettings(isolation={isolation!r}, tmp_size='64M', "
        f"protect_system={protect_system!r}, limits=sandbox.SandboxLimits(tasks_max=256))\n"
        "sandbox.run(view, settings, sys.argv[2:])\n"
    )
    environment = {
        **os.environ,
        "HOME": str(home),
        "OTHER": str(other),
        "CONTROL": str(control),
        "PYTHONPATH": str(Path(__file__).resolve().parents[2]),
    }
    try:
        result = subprocess.run(
            [sys.executable, "-c", script, str(box_base), "bash", "-c", _CONTROL_PROBE],
            capture_output=True,
            text=True,
            timeout=120,
            env=environment,
            check=False,
        )
    finally:
        subprocess.run(["systemctl", "--user", "stop", sandbox.slice_name("agent", slot)], capture_output=True, check=False)
    assert result.returncode == 0, result.stderr
    assert _probe_results(result.stdout) == {
        "own_slot_write": True,
        "other_slot_write": False,
        "other_slot_read": True,
        "registry_write": False,
        "registry_new_file": False,
        "registry_read": True,
        "other_layer_hidden": True,
        "images_hidden": True,
        "home_new_file": True,
    }, result.stdout + result.stderr
    assert (other / "secret.txt").read_text(encoding="utf-8") == "other slot's work\n"
    assert not (control / "stray").exists()
    assert (state / "home" / "new-top-level").exists()


def test_image_records_cannot_redirect_a_slot(tmp_path: Path) -> None:
    """IMAGE.json is untrusted: its location must be where wrkslots itself would put the slot."""

    control = tmp_path / "control"
    directory = slotimage.image_directory(control, "agent", "s1")
    directory.mkdir(parents=True)
    slots = control / "slots"
    record = {
        "schema": slotimage.SCHEMA, "slot": "s1", "slot_type": "agent", "ceiling_bytes": 1,
        "state_ceiling_bytes": 1, "backend": "kernel", "created_at": "now", "phase": "ready",
    }

    def write(location: str, **changes: object) -> None:
        (directory / slotimage.RECORD_NAME).write_text(
            json.dumps({**record, "location": location, **changes}), encoding="utf-8"
        )

    write(str(slots / "s1"))
    with pytest.raises(slotimage.ImageError, match="no slot layout is registered"):
        slotimage.read_record(directory)
    slotimage.register_layout(control, lambda _slot_type, slot: slots / slot)
    for accepted in (
        slots / "s1",
        slots / ".s1.fenced.1.0123456789abcdef0123456789abcdef",
        directory.parent / ".s1.convert-0123abcd",
    ):
        write(str(accepted))
        assert slotimage.read_record(directory).location == accepted
    for refused in ("/usr", str(slots / "s2"), str(slots / "s1" / "sub"), str(slots / ".s2.fenced.1.x"),
                    str(slots / ".." / "slots" / "s1"), str(tmp_path / ".s1.fenced.1.x")):
        write(refused)
        with pytest.raises(slotimage.ImageError):
            slotimage.read_record(directory)
    write(str(slots / "s1"), slot="s2")
    with pytest.raises(slotimage.ImageError, match="names another slot"):
        slotimage.read_record(directory)


def test_image_helper_refuses_paths_it_should_not_touch(tmp_path: Path) -> None:
    """The root helper checks each path through its own descriptor before acting."""

    if subprocess.run(["sudo", "-n", "true"], capture_output=True, check=False).returncode:
        pytest.skip("needs passwordless sudo")
    helper = Path(slotimage.__file__).with_name("imagehelper.py")

    def run(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sudo", "-n", "--", os.path.realpath(sys.executable), "-I", str(helper), *arguments],
            capture_output=True,
            text=True,
            check=False,
            timeout=60,
        )

    image = tmp_path / "x.img"
    image.write_bytes(b"")
    target = tmp_path / "target"
    target.mkdir()
    link = tmp_path / "link"
    link.symlink_to(target)
    (tmp_path / "full").mkdir()
    (tmp_path / "full" / "file").write_text("x", encoding="utf-8")
    linked = tmp_path / "linked.img"
    linked.write_bytes(b"")
    hard = tmp_path / "hard.img"
    os.link(linked, hard)
    for arguments, fragment in (
        (("mount", str(image), str(link)), "Not a directory"),  # O_PATH|O_NOFOLLOW|O_DIRECTORY on a symlink
        (("mount", str(image), "/usr"), "not a directory owned by the invoking user"),
        (("mount", str(image), str(tmp_path / "full")), "not empty"),
        (("mount", str(hard), str(target)), "singly linked"),
        (("umount", "/usr"), "not the root of a loop-mounted slot image"),
        (("umount", str(target)), "not the root of a loop-mounted slot image"),
        (("trim", "/"), "not the root of a loop-mounted slot image"),
        (("rmdir-lost-found", "/"), "not the root of a loop-mounted slot image"),
    ):
        result = run(*arguments)
        assert result.returncode != 0 and fragment in result.stderr, (arguments, result.stderr)
    assert not os.path.ismount(target)


def test_build_spec_creates_the_root_spec_directory_before_the_view(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A box started before any root launch must find the spec directory already
    # present (and therefore read-only), not create it itself.
    home = _fake_home(tmp_path)
    for directory in ("slot", "repo.git", "control", "project"):
        (tmp_path / directory).mkdir(parents=True)
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(runtime))
    spec = sandbox.build_spec(_view(tmp_path), sandbox.SandboxSettings(), home, tmp_path / "slot")
    spec_directory = runtime / "wrkslots"
    assert spec_directory.is_dir() and not spec_directory.is_symlink()
    assert spec_directory.stat().st_mode & 0o777 == 0o700
    assert str(spec_directory) in spec["read_only"]  # type: ignore[operator]
    spec_directory.chmod(0o755)
    with pytest.raises(sandbox.SandboxError, match="mode 0700"):
        sandbox.build_spec(_view(tmp_path), sandbox.SandboxSettings(), home, tmp_path / "slot")


def test_image_helper_resolves_system_tools_without_path(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    from wrkslots import imagehelper

    decoy = tmp_path / "resize2fs"
    decoy.write_text("#!/bin/sh\nexit 0\n")
    decoy.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))
    try:
        resolved = imagehelper._system_tool("resize2fs")
    except imagehelper.HelperError:
        pytest.skip("resize2fs is not installed in a system directory")
    assert resolved != str(decoy) and resolved.startswith(("/usr/sbin/", "/sbin/", "/usr/bin/", "/bin/"))


# ------------------------------------------------------------ coordinator box


def _small_project(base: Path, home: Path) -> tuple[Path, dict[str, str]]:
    """A wrkslots project at base/project with one source repository, src, and no slots."""

    remote, project = base / "remote.git", base / "project"
    repository = project / "src"
    git_env = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "HOME": str(home)}

    def git(*arguments: str, cwd: Path | None = None) -> None:
        subprocess.run(["git", *arguments], cwd=cwd, check=True, capture_output=True, text=True, env=git_env)

    git("init", "--bare", "--initial-branch=main", str(remote))
    project.mkdir()
    git("clone", str(remote), str(repository))
    for key, value in (("user.name", "t"), ("user.email", "t@example.invalid")):
        git("config", key, value, cwd=repository)
    (repository / "seed").write_text("seed\n", encoding="utf-8")
    git("add", "seed", cwd=repository)
    git("commit", "-m", "seed", cwd=repository)
    git("push", "-u", "origin", "main", cwd=repository)
    (project / "ai_docs").mkdir()
    (project / "logs").mkdir()
    liveness = project / "liveness.py"
    liveness.write_text("#!/usr/bin/env python3\nraise SystemExit(1)\n", encoding="utf-8")
    liveness.chmod(0o755)
    environment = {
        **git_env,
        "PYTHONPATH": str(Path(__file__).resolve().parents[2]),
        "WRKSLOTS_INIT_REPRESENTATION": "worktree",
    }
    initialized = subprocess.run(
        [sys.executable, "-m", "wrkslots", "--machine", "testhost", "init", str(project),
         "--worktrees-dir", "worktrees", "--liveness-command", "liveness.py"],
        cwd=project, capture_output=True, text=True, timeout=120, env=environment, check=False,
    )
    assert initialized.returncode == 0, initialized.stderr
    return project, environment


def test_coordinator_box_spec_scopes(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    home = _fake_home(tmp_path)
    project, _environment = _small_project(tmp_path, home)
    monkeypatch.setenv("HOME", str(home))
    config = cli._load_config(str(project), "testhost")
    control = config.control
    settings = sandbox.SandboxSettings()
    worktrees = imagecmd.box_view(config, "lead", "worktrees")
    assert worktrees.coordinator and worktrees.slot_type == "box"
    assert worktrees.state_directory == control / "box-state" / "lead"
    spec = sandbox.build_spec(worktrees, settings, home, project)
    binds = spec["binds"]
    assert isinstance(binds, list)
    targets = [target for _source, target in binds]
    # The registry and every slot (the worktrees directory) and the source
    # repository's Git directory are writable; the rest of the project is not.
    assert str(project / "worktrees") in targets
    assert str(project / "src" / ".git") in targets
    assert str(project / "ai_docs") in targets  # blessed output
    for read_only in (project, project / "src", project / "logs", home / "work"):
        assert str(read_only) not in targets
    assert str(control) not in spec["read_only"]  # type: ignore[operator]
    masks = spec["masks"]
    assert isinstance(masks, list)
    assert str(control / "box-state") in masks and str(control / "slot-state") in masks
    assert str(control / "slot-images") not in masks  # a coordinator creates image slots
    assert str(home / ".ssh") in masks and str(home / ".netrc") in masks  # credentials stay masked
    assert spec["home_layer"] == str(control / "box-state" / "lead" / "home")
    assert str(control) in spec["propagate"]  # type: ignore[operator]
    assert sandbox.slice_name("box", "lead", parent=None).endswith("wrkslots-box-lead.slice")
    whole = imagecmd.box_view(config, "lead", "project")
    spec = sandbox.build_spec(whole, settings, home, project)
    binds = spec["binds"]
    assert isinstance(binds, list)
    targets = [target for _source, target in binds]
    assert str(project) in targets
    assert str(home / "work") not in targets
    # A slot's box still masks the images and every private layer, boxes' included.
    slot_spec = sandbox.build_spec(_view(tmp_path), settings, home, tmp_path / "slot")
    for name in ("slot-images", "slot-state", "box-state"):
        assert str(tmp_path / "control" / name) in slot_spec["masks"]  # type: ignore[operator]
    assert slot_spec["propagate"] == []
    environment = sandbox.child_environment(worktrees, settings, {"HOME": str(home)})
    assert environment["WRKSLOTS_BOX"] == "lead" and environment["WRKSLOTS_BOX_WRITABLE"] == "worktrees"
    assert "WRKSLOTS_SLOT" not in environment
    with pytest.raises(cli.Refusal):
        imagecmd.box_view(config, "../escape", "worktrees")


def test_image_mounts_route_to_the_host_only_from_a_coordinator_box(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.setattr(slotimage, "_IN_BOX_NAMESPACE", False)
    for name in ("WRKSLOTS_SANDBOX", "WRKSLOTS_BOX"):
        monkeypatch.delenv(name, raising=False)
    assert slotimage.box_kind() is None
    assert slotimage._host_argv(["sudo", "-n", "true"]) == ["sudo", "-n", "true"]
    monkeypatch.setenv("WRKSLOTS_SANDBOX", "cgroup")  # limits only: the host namespace
    assert slotimage.box_kind() is None
    # A slot's box refuses every image mount operation: its mounts are private.
    monkeypatch.setenv("WRKSLOTS_SANDBOX", "root")
    assert slotimage.box_kind() == "slot"
    with pytest.raises(slotimage.ImageError, match="inside a slot's box"):
        slotimage._host_argv(["sudo", "-n", "true"])
    with pytest.raises(slotimage.ImageError, match="inside a slot's box"):
        slotimage.resolve_backend("auto")
    with pytest.raises(slotimage.ImageError, match="inside a slot's box"):
        slotimage.remove_mount_point(tmp_path / "nothing")
    assert not slotimage.sudo_available()
    # A coordinator box routes them through the user manager, but only where it can write.
    monkeypatch.setenv("WRKSLOTS_BOX", "lead")
    assert slotimage.box_kind() == "coordinator"
    if shutil.which("systemd-run") is None:
        pytest.skip("needs systemd-run")
    argv = slotimage._host_argv(["sudo", "-n", "true"], tmp_path)
    assert argv[1:4] == ["--user", "--quiet", "--collect"] and "--wait" in argv and "--pipe" in argv
    assert argv[-3:] == ["sudo", "-n", "true"]
    read_only = tmp_path / "ro"
    read_only.mkdir(mode=0o555)
    if not os.access(read_only, os.W_OK):
        with pytest.raises(slotimage.ImageError, match="inside a slot's box"):
            slotimage._host_argv(["true"], read_only)
    # The host mount table read through the user manager matches this host's own.
    monkeypatch.setattr(slotimage, "_HOST_TABLE", None)
    if runtime:
        monkeypatch.setenv("XDG_RUNTIME_DIR", runtime)
    if subprocess.run(["systemctl", "--user", "show-environment"], capture_output=True, check=False).returncode:
        pytest.skip("the systemd user manager is not reachable")
    host = {entry.mount_point for entry in slotimage.verification_table()}
    assert Path("/") in host and Path("/proc") in host


def test_wait_for_exit_uses_the_processes_found_once() -> None:
    child = subprocess.Popen(["sleep", "0.3"])
    try:
        assert slotimage._wait_for_exit([child.pid], 10)
    finally:
        child.wait()
    lingering = subprocess.Popen(["sleep", "30"])
    try:
        assert not slotimage._wait_for_exit([lingering.pid], 0.2)
    finally:
        lingering.kill()
        lingering.wait()
    assert slotimage._wait_for_exit([], 0)


_BOX_PROBE = r"""
report() { if eval "$2" >/dev/null 2>&1; then echo "$1=yes"; else echo "$1=no"; fi; }
report registry_write 'echo x > "$PROJECT/worktrees/probe-registry"'
report slot_write 'echo x > "$PROJECT/worktrees/$SLOT/src/probe-coordinator"'
report slot_commit 'cd "$PROJECT/worktrees/$SLOT/src" && git -c user.name=t -c user.email=t@e commit -q --allow-empty -m c'
report project_file_write 'echo x > "$PROJECT/logs/probe"'
report primary_checkout_write 'echo x > "$PROJECT/src/probe"'
report output_write 'echo x > "$PROJECT/ai_docs/probe"'
report agent_registry_write 'mkdir -p "$PROJECT/.agentctl" && echo x > "$PROJECT/.agentctl/probe"'
report home_work_write 'echo x > ~/work/probe'
report home_new_file 'echo x > ~/probe-top'
report ssh_hidden '[ -z "$(ls -A ~/.ssh)" ]'
report tmp_write 'echo x > /tmp/probe'
"""


@pytest.mark.parametrize("isolation", ["userns", "root"])
@pytest.mark.parametrize("scope", ["worktrees", "project"])
def test_coordinator_box_confines_writes(box_base: Path, isolation: str, scope: str) -> None:
    _sandbox_host_or_skip(isolation)
    home = _fake_home(box_base)
    project, environment = _small_project(box_base, home)

    def wrkslots(*arguments: str, timeout: int = 300) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-m", "wrkslots", "--machine", "testhost", *arguments],
            cwd=project, capture_output=True, text=True, timeout=timeout,
            env={**environment, "PROJECT": str(project), "SLOT": "cs1"}, check=False,
        )

    created = wrkslots(
        "create", "cs1", "--slot-type", "agent", "--coordinator-authorized", "--agent", "a1", "--task", "t",
        "--purpose", "coordinator box test", "--owner-pid", str(os.getpid()), "--coordinator-pid", str(os.getpid()),
        "--repo", "src=src", "--branch", "src=agent/cs1",
    )
    assert created.returncode == 0, created.stderr
    shell = wrkslots("shell-command", "--box", "--name", "lead", "--writable", scope, "--format", "json", "--", "/bin/echo", "x")
    assert shell.returncode == 0, shell.stderr
    document = json.loads(shell.stdout)
    assert document["box"] == "lead" and document["slot_path"] == str(project)
    assert f" box --name lead --cwd {project} --writable {scope} -- /bin/echo x" in document["command"]
    name = f"pytest-{isolation}-{scope}-{os.getpid()}"
    try:
        boxed = wrkslots("box", "--name", name, "--writable", scope, "--isolation", isolation, "--tmp-size", "64M",
                         "--", "bash", "-c", _BOX_PROBE)
    finally:
        subprocess.run(["systemctl", "--user", "stop", sandbox.slice_name("box", name)], capture_output=True, check=False)
    assert boxed.returncode == 0, boxed.stderr
    whole = scope == "project"
    assert _probe_results(boxed.stdout) == {
        "registry_write": True,
        "slot_write": True,
        "slot_commit": True,
        "project_file_write": whole,
        "primary_checkout_write": whole,
        "output_write": True,
        "agent_registry_write": True,
        "home_work_write": False,
        "home_new_file": True,
        "ssh_hidden": True,
        "tmp_write": True,
    }, boxed.stdout + boxed.stderr
    layer = project / "worktrees" / "box-state" / name / "home"
    assert (layer / "probe-top").exists() and not (home / "probe-top").exists()
    assert not (home / "work" / "probe").exists()
    status = wrkslots("status")
    assert "directory-without-row" not in status.stdout + status.stderr, status.stdout


def test_coordinator_box_refuses_a_bare_slot_and_a_symlinked_agent_registry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    home = _fake_home(tmp_path)
    project, environment = _small_project(tmp_path, home)
    monkeypatch.setenv("HOME", str(home))
    config = cli._load_config(str(project), "testhost")
    view = imagecmd.box_view(config, "lead", "worktrees")
    (project / ".agentctl").symlink_to(tmp_path / "missing")
    with pytest.raises(cli.Refusal, match="symbolic link"):
        imagecmd._agent_registry(view, project)
    if not sandbox.scope_in_place_available():
        pytest.skip("shell-command needs the user manager's D-Bus API")

    def shell_command(*arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-m", "wrkslots", "--machine", "testhost", "shell-command", *arguments],
            cwd=project, capture_output=True, text=True, timeout=120, env=environment, check=False,
        )

    bare = shell_command("--box", "slot01", "--format", "json")
    assert bare.returncode != 0 and "takes no SLOT" in bare.stderr, bare.stderr
    misplaced = shell_command("slot01", "--box", "--format", "json", "--", "/bin/true")
    assert misplaced.returncode != 0 and "takes no SLOT" in misplaced.stderr, misplaced.stderr
    boxed = shell_command("--box", "--format", "json", "--", "/bin/echo", "x")
    assert boxed.returncode == 0, boxed.stderr
    assert json.loads(boxed.stdout)["command"].endswith(" -- /bin/echo x")


def test_releasing_mount_point_copies_replaces_the_empty_directory(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    for name in ("WRKSLOTS_SANDBOX", "WRKSLOTS_BOX"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setattr(slotimage, "_IN_BOX_NAMESPACE", False)
    point = tmp_path / "slot"
    point.mkdir(mode=0o750)
    point.chmod(0o750)
    before = point.stat()
    slotimage.release_mount_point_copies(point)
    after = point.stat()
    assert after.st_ino != before.st_ino and oct(after.st_mode & 0o777) == oct(0o750)
    assert sorted(entry.name for entry in tmp_path.iterdir()) == ["slot"]
    (point / "content").write_text("x", encoding="utf-8")
    slotimage.release_mount_point_copies(point)  # never replaces a directory that has content
    assert (point / "content").exists() and sorted(entry.name for entry in tmp_path.iterdir()) == ["slot"]


def test_a_box_describes_only_itself_in_its_environment(tmp_path: Path) -> None:
    # A slot box started from inside a coordinator box must not inherit the
    # coordinator identity (image commands would treat it as a coordinator box),
    # and a coordinator box must not carry a slot identity.
    parent = {
        "HOME": str(tmp_path),
        "PATH": "/usr/bin:/bin",
        "WRKSLOTS_BOX": "sub",
        "WRKSLOTS_BOX_WRITABLE": "worktrees",
        "WRKSLOTS_PROJECT_ROOT": str(tmp_path / "project"),
        "WRKSLOTS_SLOT": "stale",
    }
    slot_env = sandbox.child_environment(_view(tmp_path), sandbox.SandboxSettings(isolation="userns"), parent)
    assert "WRKSLOTS_BOX" not in slot_env and "WRKSLOTS_BOX_WRITABLE" not in slot_env
    assert slot_env["WRKSLOTS_SLOT"] == "s1"
    coordinator = dataclasses.replace(_view(tmp_path), coordinator=True)
    box_env = sandbox.child_environment(coordinator, sandbox.SandboxSettings(isolation="userns"), parent)
    assert "WRKSLOTS_SLOT" not in box_env and box_env["WRKSLOTS_BOX"] == "s1"


def test_run_refuses_a_slot_box_from_inside_a_coordinator_box(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("WRKSLOTS_BOX", "sub")
    monkeypatch.setattr(sandbox, "prepare_state", lambda *args: pytest.fail("must refuse before preparing state"))
    with pytest.raises(sandbox.SandboxError, match="from inside a coordinator box"):
        sandbox.run(_view(tmp_path), sandbox.SandboxSettings(isolation="userns"), ["true"])

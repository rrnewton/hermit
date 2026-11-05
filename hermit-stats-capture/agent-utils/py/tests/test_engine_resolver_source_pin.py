"""Mutation controls for repository-local Rust launchers and provenance."""

from __future__ import annotations

import fcntl
import os
import shutil
import stat
import subprocess
import time
from pathlib import Path


AU_ROOT = Path(__file__).resolve().parents[2]
RESOLVER = AU_ROOT / "common" / "bin" / "engine-resolver"
FINGERPRINT = AU_ROOT / "common" / "bin" / "rs-source-fingerprint"
CARGO_RUNNER = AU_ROOT / "rs" / "bin" / "cargo-runner"


def _chmod_x(path: Path) -> None:
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", *args],
        cwd=root,
        check=True,
        capture_output=True,
        env={
            **os.environ,
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_SYSTEM": "/dev/null",
        },
    )


def _run_launcher(
    root: Path,
    *,
    ensure_only: bool = False,
    extra_env: dict[str, str] | None = None,
    cwd: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    """Invoke the real resolver and tracked Cargo launcher in a tiny workspace."""
    env = {
        **os.environ,
        "CARGO_NET_OFFLINE": "true",
        "DAGRUN_ENGINE": "rust",
    }
    if ensure_only:
        env["AGENT_UTILS_RS_ENSURE_ONLY"] = "1"
    if extra_env:
        env.update(extra_env)
    command = (
        root / "rs" / "bin" / "tick-hub"
        if ensure_only
        else root / "common" / "bin" / "tick-hub"
    )
    return subprocess.run(
        [str(command)],
        cwd=cwd or root,
        capture_output=True,
        text=True,
        env=env,
        check=False,
        timeout=120,
    )


def _build_sandbox(tmp_path: Path) -> Path:
    """Create a dependency-free workspace that uses the repository's real launchers."""
    root = tmp_path / "au"
    common_bin = root / "common" / "bin"
    rust_bin = root / "rs" / "bin"
    crate = root / "rs" / "tick-hub"
    source = crate / "src"
    python_bin = root / "py" / "bin"
    common_bin.mkdir(parents=True)
    rust_bin.mkdir(parents=True)
    source.mkdir(parents=True)
    python_bin.mkdir(parents=True)

    shutil.copy2(RESOLVER, common_bin / "engine-resolver")
    shutil.copy2(FINGERPRINT, common_bin / "rs-source-fingerprint")
    shutil.copy2(CARGO_RUNNER, rust_bin / "cargo-runner")
    for script in (
        common_bin / "engine-resolver",
        common_bin / "rs-source-fingerprint",
        rust_bin / "cargo-runner",
    ):
        _chmod_x(script)
    (common_bin / "tick-hub").symlink_to("engine-resolver")
    (rust_bin / "tick-hub").symlink_to("cargo-runner")
    (python_bin / "tick-hub").write_text("#!/bin/sh\necho PYTHON_FALLBACK_RAN\n")
    _chmod_x(python_bin / "tick-hub")

    (root / "rs" / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["tick-hub"]\nresolver = "2"\n'
    )
    (crate / "Cargo.toml").write_text(
        '[package]\nname = "tick-hub"\nversion = "0.1.0"\nedition = "2021"\n'
        '[lib]\nname = "tick_hub"\npath = "src/lib.rs"\n'
        '[[bin]]\nname = "tick-hub"\npath = "src/main.rs"\n'
    )
    (source / "lib.rs").write_text('pub fn message() -> &\'static str { "v1" }\n')
    (source / "main.rs").write_text(
        "use std::process::Command;\n"
        "fn main() {\n"
        '    if std::env::var_os("AGENT_UTILS_TEST_REEXEC_CHILD").is_some() {\n'
        '        println!("child-{}", tick_hub::message());\n'
        '    } else if std::env::var_os("AGENT_UTILS_TEST_REEXEC").is_some() {\n'
        "        let status = Command::new(std::env::current_exe().unwrap())\n"
        '            .env("AGENT_UTILS_TEST_REEXEC_CHILD", "1").status().unwrap();\n'
        "        std::process::exit(status.code().unwrap_or(1));\n"
        '    } else if std::env::var_os("AGENT_UTILS_TEST_PRINT_CWD").is_some() {\n'
        '        println!("{}", std::env::current_dir().unwrap().display());\n'
        "    } else {\n"
        '        println!("{}", tick_hub::message());\n'
        "    }\n"
        "}\n"
    )
    (root / ".gitignore").write_text("/rs/target/\n")

    subprocess.run(
        [
            "cargo",
            "generate-lockfile",
            "--offline",
            "--manifest-path",
            str(root / "rs" / "Cargo.toml"),
        ],
        cwd=root,
        check=True,
        capture_output=True,
    )
    _git(root, "init", "-q")
    _git(root, "add", "-A")
    return root


def test_launcher_builds_then_reuses_verified_workspace_cache(tmp_path: Path) -> None:
    """A first run builds and stamps; an unchanged run reuses the same artifact."""
    root = _build_sandbox(tmp_path)
    first = _run_launcher(root)
    assert first.returncode == 0, first.stderr
    assert first.stdout == "v1\n"
    assert "tracked Cargo launcher" in first.stderr
    assert "cache=refreshed" in first.stderr

    ensured = _run_launcher(root, ensure_only=True)
    assert ensured.returncode == 0, ensured.stderr
    target = Path(ensured.stdout.strip())
    assert target.parents[2] == root / "rs" / "target"
    provenance = target.with_name("tick-hub.provenance")
    assert target.is_file()
    assert provenance.read_text().count("\n") == 2
    original_mtime = target.stat().st_mtime_ns
    original_bytes = target.read_bytes()

    second = _run_launcher(root)
    assert second.returncode == 0, second.stderr
    assert second.stdout == "v1\n"
    assert "cache=verified" in second.stderr
    assert target.stat().st_mtime_ns == original_mtime
    assert target.read_bytes() == original_bytes

    ensured_again = _run_launcher(root, ensure_only=True)
    assert ensured_again.returncode == 0, ensured_again.stderr
    assert ensured_again.stdout == f"{target}\n"


def test_library_change_rebuilds_unchanged_entrypoint(tmp_path: Path) -> None:
    """Content changes rebuild even when the delegated source mtime is preserved."""
    root = _build_sandbox(tmp_path)
    assert _run_launcher(root).stdout == "v1\n"
    library = root / "rs" / "tick-hub" / "src" / "lib.rs"
    original_stat = library.stat()
    library.write_text('pub fn message() -> &\'static str { "v2" }\n')
    os.utime(
        library,
        ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns),
    )

    changed = _run_launcher(root)
    assert changed.returncode == 0, changed.stderr
    assert changed.stdout == "v2\n"
    assert "cache=refreshed" in changed.stderr


def test_nonignored_untracked_source_change_rebuilds(tmp_path: Path) -> None:
    """A new module is live source before its first commit and must invalidate the cache."""
    root = _build_sandbox(tmp_path)
    library = root / "rs" / "tick-hub" / "src" / "lib.rs"
    payload = library.with_name("payload.txt")
    library.write_text(
        'pub fn message() -> &\'static str { include_str!("payload.txt") }\n'
    )
    payload.write_text("v1")
    _git(root, "add", "rs/tick-hub/src/lib.rs", "rs/tick-hub/src/payload.txt")
    assert _run_launcher(root).stdout == "v1\n"

    # Keep the same path/content in the working tree but make it ordinary untracked source.
    _git(root, "rm", "--cached", "rs/tick-hub/src/payload.txt")
    original_stat = payload.stat()
    payload.write_text("v2")
    os.utime(payload, ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns))

    changed = _run_launcher(root)
    assert changed.returncode == 0, changed.stderr
    assert changed.stdout == "v2\n"
    assert "cache=refreshed" in changed.stderr


def test_source_enumeration_failure_cannot_execute_a_cached_binary(tmp_path: Path) -> None:
    """A Git failure after partial output must not bless or execute that partial snapshot."""
    root = _build_sandbox(tmp_path)
    assert _run_launcher(root).stdout == "v1\n"

    real_git = shutil.which("git")
    assert real_git is not None
    wrapper_dir = tmp_path / "failing git"
    wrapper_dir.mkdir()
    git_wrapper = wrapper_dir / "git"
    git_wrapper.write_text(
        "#!/bin/sh\n"
        '"$AGENT_UTILS_TEST_REAL_GIT" "$@"\n'
        "exit 42\n"
    )
    _chmod_x(git_wrapper)

    failed = _run_launcher(
        root,
        extra_env={
            "PATH": str(wrapper_dir) + os.pathsep + os.environ["PATH"],
            "AGENT_UTILS_TEST_REAL_GIT": real_git,
        },
    )

    assert failed.returncode != 0
    assert failed.stdout == ""
    assert "could not enumerate rs/ source" in failed.stderr


def test_replaced_or_unstamped_artifact_is_rebuilt_not_executed(tmp_path: Path) -> None:
    """A planted binary and a missing stamp both fail closed into a Cargo rebuild."""
    root = _build_sandbox(tmp_path)
    assert _run_launcher(root).stdout == "v1\n"
    target = Path(_run_launcher(root, ensure_only=True).stdout.strip())
    provenance = target.with_name("tick-hub.provenance")

    target.write_text("#!/bin/sh\necho PLANTED_RUNNER\n")
    _chmod_x(target)
    repaired = _run_launcher(root)
    assert repaired.returncode == 0, repaired.stderr
    assert repaired.stdout == "v1\n"
    assert "PLANTED_RUNNER" not in repaired.stdout
    assert "discarding unverified" in repaired.stderr

    provenance.unlink()
    unstamped = _run_launcher(root)
    assert unstamped.returncode == 0, unstamped.stderr
    assert unstamped.stdout == "v1\n"
    assert "discarding unverified" in unstamped.stderr

    target.chmod(target.stat().st_mode & ~(stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH))
    mode_repaired = _run_launcher(root)
    assert mode_repaired.returncode == 0, mode_repaired.stderr
    assert mode_repaired.stdout == "v1\n"
    assert "discarding unverified" in mode_repaired.stderr


def test_named_snapshot_supports_current_exe_reexecution(tmp_path: Path) -> None:
    """The development launcher preserves the stable path required by containment re-exec."""
    root = _build_sandbox(tmp_path)
    reexecuted = _run_launcher(root, extra_env={"AGENT_UTILS_TEST_REEXEC": "1"})
    assert reexecuted.returncode == 0, reexecuted.stderr
    assert reexecuted.stdout == "child-v1\n"


def test_launcher_ignores_unrelated_cwd_config_and_restores_tool_cwd(tmp_path: Path) -> None:
    """Cargo resolves from the checkout, while the final utility sees its caller's directory."""
    root = _build_sandbox(tmp_path / "checkout with spaces")
    outside = tmp_path / "unrelated project"
    cargo_config = outside / ".cargo" / "config.toml"
    cargo_config.parent.mkdir(parents=True)
    cargo_config.write_text('[build]\nrustc = "/definitely/not/a/rustc"\n')
    real_rustc = shutil.which("rustc")
    assert real_rustc is not None
    spaced_rustc = outside / "tool chain" / "rustc with space"
    spaced_rustc.parent.mkdir()
    spaced_rustc.write_text(
        '#!/bin/sh\nexec "$AGENT_UTILS_TEST_REAL_RUSTC" "$@"\n'
    )
    _chmod_x(spaced_rustc)

    launched = _run_launcher(
        root,
        cwd=outside,
        extra_env={
            "AGENT_UTILS_TEST_PRINT_CWD": "1",
            "AGENT_UTILS_TEST_REAL_RUSTC": real_rustc,
            "RUSTC": str(spaced_rustc),
        },
    )
    assert launched.returncode == 0, launched.stderr
    assert launched.stdout.strip() == str(outside)


def test_launcher_ignores_checkout_ancestor_cargo_config_for_cached_and_refreshed_builds(
    tmp_path: Path,
) -> None:
    """An enclosing consumer repository's Cargo policy cannot poison agent-utils builds."""
    ambient = tmp_path / "ambient parent"
    root = _build_sandbox(ambient / "checkout")
    first = _run_launcher(root)
    assert first.returncode == 0, first.stderr
    assert first.stdout == "v1\n"

    cargo_config = ambient / ".cargo" / "config.toml"
    cargo_config.parent.mkdir()
    cargo_config.write_text(
        '[build]\nrustc = "/definitely/not/a/rustc"\n'
        '[profile.release]\ntrim-paths = "all"\n'
    )
    (ambient / "rust-toolchain.toml").write_text(
        '[toolchain]\nchannel = "agent-utils-must-not-load-this-toolchain"\n'
    )

    cached = _run_launcher(root)
    assert cached.returncode == 0, cached.stderr
    assert cached.stdout == "v1\n"
    assert "cache=verified" in cached.stderr

    (root / "rs" / "tick-hub" / "src" / "lib.rs").write_text(
        'pub fn message() -> &\'static str { "v2" }\n'
    )
    refreshed = _run_launcher(root)
    assert refreshed.returncode == 0, refreshed.stderr
    assert refreshed.stdout == "v2\n"
    assert "cache=refreshed" in refreshed.stderr


def test_launcher_absolutizes_an_explicit_checkout_relative_rustc(tmp_path: Path) -> None:
    """Neutral Cargo cwd preserves a caller's explicit checkout-relative compiler wrapper."""
    root = _build_sandbox(tmp_path)
    real_rustc = shutil.which("rustc")
    assert real_rustc is not None
    wrapper = root / "tool chain" / "rustc wrapper"
    wrapper.parent.mkdir()
    wrapper.write_text('#!/bin/sh\nexec "$AGENT_UTILS_TEST_REAL_RUSTC" "$@"\n')
    _chmod_x(wrapper)

    launched = _run_launcher(
        root,
        extra_env={
            "RUSTC": str(wrapper.relative_to(root)),
            "AGENT_UTILS_TEST_REAL_RUSTC": real_rustc,
        },
    )

    assert launched.returncode == 0, launched.stderr
    assert launched.stdout == "v1\n"


def test_source_change_during_snapshot_retries_before_execution(tmp_path: Path) -> None:
    """A source edit while the executable copy is paused cannot launch the earlier build."""
    root = _build_sandbox(tmp_path)
    ensured = _run_launcher(root, ensure_only=True)
    assert ensured.returncode == 0, ensured.stderr

    real_cp = shutil.which("cp")
    assert real_cp is not None
    wrapper_dir = tmp_path / "wrapped tools"
    wrapper_dir.mkdir()
    cp_wrapper = wrapper_dir / "cp"
    cp_wrapper.write_text(
        "#!/bin/sh\n"
        ': > "$AGENT_UTILS_TEST_CP_STARTED"\n'
        'while [ ! -e "$AGENT_UTILS_TEST_CP_RELEASE" ]; do sleep 0.02; done\n'
        'exec "$AGENT_UTILS_TEST_REAL_CP" "$@"\n'
    )
    _chmod_x(cp_wrapper)
    started = tmp_path / "cp-started"
    release = tmp_path / "cp-release"
    env = {
        **os.environ,
        "CARGO_NET_OFFLINE": "true",
        "DAGRUN_ENGINE": "rust",
        "PATH": str(wrapper_dir) + os.pathsep + os.environ["PATH"],
        "AGENT_UTILS_TEST_CP_STARTED": str(started),
        "AGENT_UTILS_TEST_CP_RELEASE": str(release),
        "AGENT_UTILS_TEST_REAL_CP": real_cp,
    }
    launched = subprocess.Popen(
        [str(root / "common" / "bin" / "tick-hub")],
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    deadline = time.monotonic() + 10
    while not started.exists():
        if launched.poll() is not None:
            stdout, stderr = launched.communicate()
            raise AssertionError(f"launcher exited before snapshot pause: {stdout}\n{stderr}")
        if time.monotonic() >= deadline:
            launched.kill()
            raise AssertionError("launcher never reached snapshot publication")
        time.sleep(0.02)

    (root / "rs" / "tick-hub" / "src" / "lib.rs").write_text(
        'pub fn message() -> &\'static str { "v2" }\n'
    )
    release.touch()
    stdout, stderr = launched.communicate(timeout=30)
    assert launched.returncode == 0, stderr
    assert stdout == "v2\n"
    assert "source changed while snapshotting" in stderr


def test_coordinated_clean_waits_for_build_and_does_not_break_launch(tmp_path: Path) -> None:
    """The outside-target cache lock serializes clean through named snapshot publication."""
    root = _build_sandbox(tmp_path)
    build_script = root / "rs" / "tick-hub" / "build.rs"
    build_script.write_text(
        "fn main() { std::thread::sleep(std::time::Duration::from_millis(1200)); }\n"
    )
    _git(root, "add", "rs/tick-hub/build.rs")
    env = {
        **os.environ,
        "CARGO_NET_OFFLINE": "true",
        "DAGRUN_ENGINE": "rust",
    }
    launched = subprocess.Popen(
        [str(root / "common" / "bin" / "tick-hub")],
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    lock_path = root / "rs" / ".agent-utils-locks" / "cache.lock"
    deadline = time.monotonic() + 10
    while True:
        if lock_path.exists():
            with lock_path.open("a+b") as probe:
                try:
                    fcntl.flock(probe.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    break
                else:
                    fcntl.flock(probe.fileno(), fcntl.LOCK_UN)
        if time.monotonic() >= deadline:
            launched.kill()
            raise AssertionError("launcher never acquired the cache lock")
        time.sleep(0.02)

    cleaned = subprocess.Popen(
        ["flock", str(lock_path), "rm", "-rf", "--", str(root / "rs" / "target")],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    time.sleep(0.2)
    assert cleaned.poll() is None, "clean bypassed the launcher's held lock"

    stdout, stderr = launched.communicate(timeout=30)
    clean_stdout, clean_stderr = cleaned.communicate(timeout=30)
    assert launched.returncode == 0, stderr
    assert stdout == "v1\n"
    assert cleaned.returncode == 0, clean_stdout + clean_stderr
    assert not (root / "rs" / "target").exists()


def test_ambient_cargo_target_cannot_redirect_build_away_from_executed_binary(
    tmp_path: Path,
) -> None:
    """Environment and config target defaults cannot make the launcher run an older path."""
    host = next(
        line.removeprefix("host: ")
        for line in subprocess.run(
            [os.environ.get("RUSTC", "rustc"), "-vV"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        if line.startswith("host: ")
    )

    env_root = _build_sandbox(tmp_path / "env")
    assert _run_launcher(env_root).stdout == "v1\n"
    (env_root / "rs" / "tick-hub" / "src" / "lib.rs").write_text(
        'pub fn message() -> &\'static str { "v2-env" }\n'
    )
    env_changed = _run_launcher(
        env_root, extra_env={"CARGO_BUILD_TARGET": host}
    )
    assert env_changed.returncode == 0, env_changed.stderr
    assert env_changed.stdout == "v2-env\n"

    config_root = _build_sandbox(tmp_path / "config")
    assert _run_launcher(config_root).stdout == "v1\n"
    (config_root / "rs" / "tick-hub" / "src" / "lib.rs").write_text(
        'pub fn message() -> &\'static str { "v2-config" }\n'
    )
    cargo_config = config_root / ".cargo" / "config.toml"
    cargo_config.parent.mkdir()
    cargo_config.write_text(f'[build]\ntarget = "{host}"\n')
    config_changed = _run_launcher(config_root)
    assert config_changed.returncode == 0, config_changed.stderr
    assert config_changed.stdout == "v2-config\n"


def test_build_failure_never_runs_previous_binary_or_python(tmp_path: Path) -> None:
    """Broken newer source cannot fall back to either cached Rust or Python code."""
    root = _build_sandbox(tmp_path)
    assert _run_launcher(root).stdout == "v1\n"
    (root / "rs" / "tick-hub" / "src" / "lib.rs").write_text("this is not Rust\n")

    failed = _run_launcher(root)
    assert failed.returncode != 0
    assert "Cargo build failed; no cached Rust artifact was executed" in failed.stderr
    assert "v1" not in failed.stdout
    assert "PYTHON_FALLBACK_RAN" not in failed.stdout

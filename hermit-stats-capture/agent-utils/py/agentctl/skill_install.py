"""Install the bundled agentctl skill without replacing unreviewed content."""
from __future__ import annotations

import json
import os
import pwd
import selectors
import signal
import stat
import subprocess
import tempfile
import time
from importlib.resources import files
from pathlib import Path
from typing import Sequence

from agentctl.errors import AgentDeliveryError

HARNESSES = ("codex", "claude", "muse")
_INSTALL_OUTPUT_BYTES = 1 << 20
_INSTALL_TIMEOUT = 30.0


def _root(harness: str) -> Path:
    override = os.environ.get(f"AGENTCTL_{harness.upper()}_SKILLS_DIR")
    if override is not None:
        root = Path(override)
        if not root.is_absolute():
            raise AgentDeliveryError(f"{harness} skill directory override must be absolute")
        return root
    home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    return home / {"codex": ".codex", "claude": ".claude"}[harness] / "skills"


def _ensure_real_directory(path: Path) -> None:
    """Create missing components while refusing every symlink in the destination path."""
    if not path.is_absolute():
        raise AgentDeliveryError(f"skill destination must be absolute: {path}")
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            try:
                current.mkdir(mode=0o700)
            except OSError as exc:
                raise AgentDeliveryError(
                    f"cannot create skill destination {current}: {exc}"
                ) from exc
            continue
        except OSError as exc:
            raise AgentDeliveryError(f"cannot inspect skill destination {current}: {exc}") from exc
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise AgentDeliveryError(f"refusing non-directory skill destination: {current}")


def _existing(path: Path, content: str, *, force: bool) -> str | None:
    """Return unchanged, or validate that native/direct replacement is allowed."""
    if not os.path.lexists(path):
        return None
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise AgentDeliveryError(f"refusing non-regular skill file: {path}")
    try:
        existing = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise AgentDeliveryError(f"cannot inspect installed skill {path}: {exc}") from exc
    if existing == content:
        return "unchanged"
    if not force:
        raise AgentDeliveryError(f"refusing to overwrite divergent skill {path}; inspect it or pass --force")
    return None


def _refuse_directory_symlink(path: Path) -> None:
    if not os.path.lexists(path):
        return
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise AgentDeliveryError(f"refusing non-directory skill destination: {path}")


def _write(path: Path, content: str, *, force: bool) -> str:
    directory = path.parent
    _ensure_real_directory(directory)
    outcome = _existing(path, content, force=force)
    if outcome is not None:
        return outcome
    fd, temporary = tempfile.mkstemp(prefix=".SKILL.md.", dir=directory, text=True)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
    return "installed"


def _muse_config_home() -> Path:
    configured = os.environ.get("XDG_CONFIG_HOME")
    if configured is not None:
        root = Path(configured)
        if not root.is_absolute():
            raise AgentDeliveryError("XDG_CONFIG_HOME must be absolute for Muse skill installation")
        return root / "muse"
    return Path(pwd.getpwuid(os.getuid()).pw_dir) / ".config" / "muse"


def _muse_executable_candidates() -> tuple[Path, ...]:
    home = Path(pwd.getpwuid(os.getuid()).pw_dir)
    return (
        Path("/usr/local/bin/muse"),
        Path("/usr/bin/muse"),
        home / ".local/bin/muse",
        home / "bin/muse",
        home / ".cargo/bin/muse",
    )


def _muse_executable() -> str:
    configured = os.environ.get("AGENTCTL_MUSE_BIN")
    candidate = configured if configured is not None else next((
        str(path) for path in _muse_executable_candidates()
        if path.is_file() and os.access(path, os.X_OK)
    ), None)
    if not candidate:
        raise AgentDeliveryError("Muse executable not found; install Muse before installing its skill")
    path = Path(candidate)
    if not path.is_absolute():
        raise AgentDeliveryError("AGENTCTL_MUSE_BIN must be an absolute path")
    resolved = Path(os.path.realpath(path))
    try:
        metadata = resolved.stat()
    except OSError as exc:
        raise AgentDeliveryError(f"cannot inspect Muse executable {resolved}: {exc}") from exc
    if (not stat.S_ISREG(metadata.st_mode) or not os.access(resolved, os.X_OK)
            or metadata.st_mode & (stat.S_IWGRP | stat.S_IWOTH)):
        raise AgentDeliveryError(f"refusing unsafe Muse executable: {resolved}")
    return str(resolved)


def _run_muse_installer(command: list[str], environment: dict[str, str]) -> tuple[int, str, str]:
    """Run the native manager with bounded pipes, time, and process-group cleanup."""
    try:
        process = subprocess.Popen(
            command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, env=environment, start_new_session=True,
        )
    except OSError as exc:
        raise AgentDeliveryError(f"Muse skill installation failed: {exc}") from exc
    assert process.stdout is not None and process.stderr is not None
    streams = (process.stdout, process.stderr)
    buffers = {stream.fileno(): bytearray() for stream in streams}
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + _INSTALL_TIMEOUT
    try:
        for stream in streams:
            os.set_blocking(stream.fileno(), False)
            selector.register(stream.fileno(), selectors.EVENT_READ)
        while selector.get_map():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, _INSTALL_TIMEOUT)
            for key, _events in selector.select(min(0.1, remaining)):
                try:
                    chunk = os.read(key.fd, 65_536)
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(key.fd)
                    continue
                buffer = buffers[key.fd]
                buffer.extend(chunk)
                if len(buffer) > _INSTALL_OUTPUT_BYTES:
                    raise AgentDeliveryError(
                        f"Muse skill installation output exceeds {_INSTALL_OUTPUT_BYTES} bytes"
                    )
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise subprocess.TimeoutExpired(command, _INSTALL_TIMEOUT)
        returncode = process.wait(timeout=remaining)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            process.kill()
        except ProcessLookupError:
            pass
        process.wait()
        raise
    finally:
        selector.close()
    try:
        stdout = bytes(buffers[process.stdout.fileno()]).decode("utf-8", errors="strict")
        stderr = bytes(buffers[process.stderr.fileno()]).decode("utf-8", errors="strict")
    except UnicodeError as exc:
        raise AgentDeliveryError(f"Muse skill installation returned invalid UTF-8: {exc}") from exc
    return returncode, stdout, stderr


def _install_muse(content: str, *, force: bool) -> str:
    # Validate code before making any configuration directories.
    executable = _muse_executable()
    config_home = _muse_config_home()
    skills = config_home / "skills"
    _ensure_real_directory(skills)
    destination = skills / "agentctl" / "SKILL.md"
    _refuse_directory_symlink(destination.parent)
    outcome = _existing(destination, content, force=force)
    if outcome is not None:
        return outcome
    with tempfile.TemporaryDirectory(prefix="agentctl-muse-skill-") as temporary:
        source = Path(temporary) / "agentctl"
        source.mkdir(mode=0o700)
        source_file = source / "SKILL.md"
        source_file.write_text(content, encoding="utf-8")
        source_file.chmod(0o600)
        command = [
            executable, "skills", "install", str(source), "--scope", "user",
            "--name", "agentctl", "--json",
        ]
        if force:
            command.append("--force")
        environment = dict(os.environ)
        environment["XDG_CONFIG_HOME"] = str(config_home.parent)
        try:
            returncode, stdout, stderr = _run_muse_installer(command, environment)
        except (OSError, UnicodeError, subprocess.TimeoutExpired) as exc:
            raise AgentDeliveryError(f"Muse skill installation failed: {exc}") from exc
    if returncode != 0:
        detail = (stderr or stdout).strip()[-4000:]
        raise AgentDeliveryError(f"Muse skill installation failed: {detail or returncode}")
    try:
        receipt = json.loads(stdout)
    except (json.JSONDecodeError, RecursionError) as exc:
        raise AgentDeliveryError(f"Muse skill installation returned invalid JSON: {exc}") from exc
    if (not isinstance(receipt, dict) or not isinstance(receipt.get("installed"), dict)
            or receipt["installed"].get("id") != "agentctl"):
        raise AgentDeliveryError("Muse skill installation returned no agentctl receipt")
    if _existing(destination, content, force=False) != "unchanged":
        raise AgentDeliveryError("Muse reported installation without the expected managed skill")
    return "installed"


def install_skill(harnesses: Sequence[str], *, force: bool = False) -> dict[str, list[str]]:
    """Install one byte-identical bundled skill for each selected harness."""
    selected = tuple(dict.fromkeys(harnesses or HARNESSES))
    invalid = sorted(set(selected) - set(HARNESSES))
    if invalid:
        raise AgentDeliveryError(f"unsupported skill harnesses: {', '.join(invalid)}")
    content = files("agentctl").joinpath("AGENTCTL_SKILL.md").read_text(encoding="utf-8")
    result: dict[str, list[str]] = {"installed": [], "unchanged": []}
    for harness in selected:
        if harness == "muse":
            outcome = _install_muse(content, force=force)
        else:
            destination = _root(harness) / "agentctl" / "SKILL.md"
            outcome = _write(destination, content, force=force)
        result[outcome].append(harness)
    return result

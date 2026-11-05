"""Generic binding tests; the consuming producer owns its private protocol tests."""

from __future__ import annotations

import fcntl
import json
import os
from collections.abc import Sequence
from pathlib import Path

import pytest

from wrkslots import cli


# Deliberately a contract stub: it exposes controlled typed values to exercise
# the bridge's comparisons. It is not evidence for a producer's verifier.
CONTRACT_MODULE = '''
import dataclasses, json, re
from pathlib import Path
TOOL_AUTHORITY_ENV = "FIXTURE_TOOL_AUTHORITY"
def proc_fd_identity(path, *, role):
    match = re.fullmatch(r"/proc/([1-9][0-9]*)/fd/([0-9]+)", str(path))
    if match is None:
        raise ValueError("invalid descriptor")
    return int(match[1]), int(match[2])
def read_immutable_tool_authority(tool_root, state_root, environment):
    assert tool_root.is_dir()
    record = json.loads(Path(environment[TOOL_AUTHORITY_ENV]).read_bytes())
    values = record["fixture_values"]
    cls = dataclasses.make_dataclass("FixtureAuthority", values)
    return cls(**values)
'''


def binding_fixture(
    tmp_path: Path,
    *,
    changes: dict[str, object] | None = None,
    remove: tuple[str, ...] = (),
    sealed: bool = True,
    oversized: bool = False,
    locator_changes: dict[str, object] | None = None,
) -> tuple[Path, dict[str, dict[str, object]], tuple[int, int]]:
    root = tmp_path / "tool"
    state = tmp_path / "state"
    root.mkdir()
    state.mkdir()
    module = tmp_path / "immutable_tool_authority.py"
    module.write_text(CONTRACT_MODULE)
    root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    authority_fd = os.memfd_create("fixture-authority", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    expected: dict[str, object] = {
        "authority_head": "a" * 40, "consumer_head": "b" * 40,
        "agent_head": "c" * 40, "state_root": str(state),
        "root_dev": root.stat().st_dev, "root_ino": root.stat().st_ino,
        "state_dev": state.stat().st_dev, "state_ino": state.stat().st_ino,
    }
    values: dict[str, object] = {
        "parent_sha": expected["authority_head"], "product_sha": expected["consumer_head"],
        "agent_utils_sha": expected["agent_head"],
        **{key: value for key, value in expected.items() if key.endswith(("_dev", "_ino"))},
        "state_root": str(state),
        **(changes or {}),
    }
    for name in remove:
        del values[name]
    record = {"holder_pid": os.getpid(), "root_fd": root_fd, "fixture_values": values}
    record.update(locator_changes or {})
    if oversized:
        record["oversized"] = "x" * 4096
    os.write(authority_fd, json.dumps(record).encode())
    os.fchmod(authority_fd, 0o400)
    if sealed:
        fcntl.fcntl(authority_fd, fcntl.F_ADD_SEALS,
                    fcntl.F_SEAL_SEAL | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_GROW | fcntl.F_SEAL_WRITE)
    request: dict[str, dict[str, object]] = {"environment": {"FIXTURE_TOOL_AUTHORITY": f"/proc/{os.getpid()}/fd/{authority_fd}"}, "expected": expected}
    return module, request, (root_fd, authority_fd)


def inspect_contract(
    module: Path, request: dict[str, dict[str, object]]
) -> tuple[int, bytes, bytes]:
    python = cli._root_owned_executable(Path("/usr/bin/python3").resolve(), "Python")
    return cli._run_bounded_read_only_command(
        (str(python.path), "-I", "-B", "-c", cli._FROZEN_TOOL_AUTHORITY_BOOTSTRAP, str(module)),
        timeout_seconds=10, stdout_limit=16 * 1024, stderr_limit=16 * 1024,
        input_data=json.dumps(request).encode(), trusted_executables=(python,),
    )


def test_generic_binding_uses_held_parent_descriptors_without_inheriting_them(tmp_path: Path) -> None:
    module, request, descriptors = binding_fixture(tmp_path)
    try:
        result, stdout, stderr = inspect_contract(module, request)
        assert result == 0, stderr.decode()
        assert stderr == b""
        assert json.loads(stdout) == request["expected"]
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


@pytest.mark.parametrize("changes,remove", [
    ({"parent_sha": "d" * 40}, ()),
    ({"product_sha": "d" * 40}, ()),
    ({"agent_utils_sha": "d" * 40}, ()),
    ({"product_sha": "b" * 39}, ()),
    ({"product_sha": True}, ()),
    ({"extra_product_sha": "b" * 40}, ()),
    ({}, ("product_sha",)),
    ({}, ("parent_sha",)),
    ({"root_ino": 1}, ()),
    ({"state_ino": 1}, ()),
    ({"root_dev": True}, ()),
    ({"state_root": "/unrelated-state"}, ()),
])
def test_generic_binding_refuses_ambiguous_pins_and_wrong_loaded_identities(
    tmp_path: Path, changes: dict[str, object], remove: tuple[str, ...]
) -> None:
    module, request, descriptors = binding_fixture(tmp_path, changes=changes, remove=remove)
    try:
        result, stdout, stderr = inspect_contract(module, request)
        assert result != 0
        assert stdout == b""
        assert b"ValueError" in stderr
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


@pytest.mark.parametrize("failure", ["unsealed", "oversized", "closed", "regular-file", "non-capability"])
def test_descriptor_locator_cannot_authorize_unsealed_unbounded_or_missing_evidence(
    tmp_path: Path, failure: str
) -> None:
    module, request, descriptors = binding_fixture(
        tmp_path, sealed=failure != "unsealed", oversized=failure == "oversized"
    )
    root_fd, authority_fd = descriptors
    regular_fd: int | None = None
    try:
        if failure == "closed":
            os.close(authority_fd)
        elif failure == "regular-file":
            record = tmp_path / "regular-record"
            record.write_text("{}")
            regular_fd = os.open(record, os.O_RDONLY | os.O_CLOEXEC)
            request["environment"]["FIXTURE_TOOL_AUTHORITY"] = f"/proc/{os.getpid()}/fd/{regular_fd}"
        elif failure == "non-capability":
            request["environment"]["FIXTURE_TOOL_AUTHORITY"] = str(module)
        result, stdout, stderr = inspect_contract(module, request)
        assert result != 0
        assert stdout == b""
        assert stderr
    finally:
        os.close(root_fd)
        if failure != "closed":
            os.close(authority_fd)
        if regular_fd is not None:
            os.close(regular_fd)


@pytest.mark.parametrize("changes", [
    {"holder_pid": True}, {"holder_pid": 1}, {"root_fd": True},
    {"root_fd": -1}, {"root_fd": "1"},
])
def test_locator_fields_remain_untrusted_before_real_verifier(
    tmp_path: Path, changes: dict[str, object]
) -> None:
    module, request, descriptors = binding_fixture(tmp_path, locator_changes=changes)
    try:
        result, stdout, stderr = inspect_contract(module, request)
        assert result != 0
        assert stdout == b""
        assert b"invalid holder or root identity" in stderr
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


@pytest.mark.parametrize("mutation", [
    "none", "module-before", "module-after", "module-symlink", "tool-directory",
    "state-directory", "repository-head", "reply", "stderr", "timeout",
])
def test_pinned_verifier_snapshot_and_bindings_are_rechecked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mutation: str
) -> None:
    module, request, descriptors = binding_fixture(tmp_path)
    authority = tmp_path / "tool"
    state = tmp_path / "state"
    consumer = authority / "consumer"
    agent = consumer / "agent-utils"
    agent.mkdir(parents=True)
    pinned_module = authority / cli._FROZEN_TOOL_AUTHORITY_MODULE
    pinned_module.parent.mkdir(parents=True)
    pinned_module.write_bytes(module.read_bytes())
    pinned_bytes = module.read_bytes()
    heads = {authority: "a" * 40, consumer: "b" * 40, agent: "c" * 40}
    monkeypatch.setenv("FIXTURE_TOOL_AUTHORITY", str(request["environment"]["FIXTURE_TOOL_AUTHORITY"]))

    def capture(
        repository: Path, arguments: Sequence[str], *, label: str,
        stdout_limit: int = 1024 * 1024,
    ) -> bytes:
        del label, stdout_limit
        assert repository == authority
        assert tuple(arguments) == ("cat-file", "blob", f"{'a' * 40}:{cli._FROZEN_TOOL_AUTHORITY_MODULE}")
        return pinned_bytes

    monkeypatch.setattr(cli, "_trusted_git_capture", capture)
    monkeypatch.setattr(cli, "_trusted_git_head", lambda checkout, label: heads[checkout])
    original_run = cli._run_bounded_read_only_command
    invoked: list[str] = []

    def observed_run(
        command: Sequence[str], *, timeout_seconds: float, stdout_limit: int,
        stderr_limit: int, input_data: bytes,
        trusted_executables: Sequence[cli._TrustedExecutablePath],
    ) -> tuple[int, bytes, bytes]:
        invoked.append("verifier")
        assert timeout_seconds == 15
        assert Path(command[-1]).read_bytes() == pinned_bytes
        assert Path(command[-1]) != pinned_module
        result = original_run(
            command, timeout_seconds=timeout_seconds, stdout_limit=stdout_limit,
            stderr_limit=stderr_limit, input_data=input_data,
            trusted_executables=trusted_executables,
        )
        assert result[0] == 0, result[2].decode()
        if mutation == "module-after":
            pinned_module.write_bytes(pinned_bytes + b"\n# changed\n")
        elif mutation == "tool-directory":
            authority.rename(tmp_path / "retained-tool")
            authority.mkdir()
        elif mutation == "state-directory":
            state.rename(tmp_path / "retained-state")
            state.mkdir()
        elif mutation == "repository-head":
            heads[consumer] = "d" * 40
        elif mutation == "reply":
            return 0, b"{}", b""
        elif mutation == "stderr":
            return 0, result[1], b"uncertain verifier observation"
        elif mutation == "timeout":
            raise cli.Refusal("bounded verifier timeout")
        return result

    monkeypatch.setattr(cli, "_run_bounded_read_only_command", observed_run)
    config = cli.Config(
        root=state, config_path=state / ".wrkslots.yml", worktrees=state / "worktrees",
        control=state / "worktrees", machine="fixture", default_remote="origin",
        default_landed_ref="refs/remotes/origin/main", heartbeat_ttl_seconds=60,
        liveness_command=state / "liveness.py",
    )
    if mutation == "module-before":
        pinned_module.write_bytes(pinned_bytes + b"\n# changed\n")
    elif mutation == "module-symlink":
        pinned_module.unlink()
        pinned_module.symlink_to(module)
    try:
        if mutation == "none":
            cli._authenticate_frozen_tool_state_root(
                config, authority, consumer, agent, "a" * 40, "b" * 40, "c" * 40
            )
        else:
            with pytest.raises(cli.Refusal):
                cli._authenticate_frozen_tool_state_root(
                    config, authority, consumer, agent, "a" * 40, "b" * 40, "c" * 40
                )
        assert invoked == ([] if mutation in {"module-before", "module-symlink"} else ["verifier"])
    finally:
        for descriptor in descriptors:
            os.close(descriptor)

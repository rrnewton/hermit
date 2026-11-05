"""Exercise slot networking through a command-scoped host wrapper."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import textwrap
from collections.abc import Mapping, Sequence
from pathlib import Path

import pytest

from wrkslots import cli as wrkslots
from wrkslots.tests import test_lifecycle as lifecycle


def install_proxy_wrapper(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    tools = tmp_path / "network-tools"
    tools.mkdir()
    log = tmp_path / "proxy-calls.jsonl"
    wrapper = tools / "with-proxy"
    wrapper.write_text(
        f"#!{sys.executable}\n"
        + textwrap.dedent(
            """\
            import json
            import os
            import sys
            from pathlib import Path

            command = sys.argv[1:]
            with Path(os.environ['WRKSLOTS_TEST_PROXY_LOG']).open('a') as output:
                output.write(json.dumps(command) + '\\n')
            if command[0] == 'git':
                if 'ls-remote' in command:
                    assert os.environ['GIT_DIR'] == '/dev/null'
                else:
                    assert 'GIT_DIR' not in os.environ
                assert 'GIT_CONFIG_COUNT' not in os.environ
                assert 'WRKSLOTS_NETWORK_CONFIG_SHA256' not in os.environ
                assert os.environ['GIT_CONFIG_GLOBAL'] == '/dev/null'
                assert os.environ['GIT_CONFIG_SYSTEM'] == '/dev/null'
                assert os.environ['GIT_NO_REPLACE_OBJECTS'] == '1'
            if os.environ.get('WRKSLOTS_TEST_PROXY_FAIL') == command[0]:
                print('test proxy refused the command', file=sys.stderr)
                raise SystemExit(17)
            os.environ['WRKSLOTS_TEST_PROXY_MARKER'] = 'via-with-proxy'
            os.execvp(command[0], command)
            """
        ),
        encoding="utf-8",
    )
    wrapper.chmod(0o755)
    monkeypatch.setenv("PATH", f"{tools}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("WRKSLOTS_TEST_PROXY_LOG", str(log))
    return log


def install_https_transport_helper(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    remote: Path,
) -> None:
    """Route an HTTPS-shaped URL through Git's real remote-helper protocol."""

    tools = tmp_path / "network-tools"
    real_git_value = shutil.which("git")
    assert real_git_value is not None
    real_git = Path(real_git_value).resolve()
    exec_path = Path(
        subprocess.run(
            [str(real_git), "--exec-path"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    )
    upload_pack = exec_path / "git-upload-pack"
    receive_pack = exec_path / "git-receive-pack"
    assert upload_pack.is_file()
    assert receive_pack.is_file()
    git_wrapper = tools / "git"
    git_wrapper.write_text(
        f"#!{sys.executable}\n"
        + textwrap.dedent(
            f"""\
            import os
            import sys

            os.environ['GIT_EXEC_PATH'] = {str(tools)!r}
            os.execv({str(real_git)!r}, [{str(real_git)!r}, *sys.argv[1:]])
            """
        ),
        encoding="utf-8",
    )
    git_wrapper.chmod(0o755)
    helper = tools / "git-remote-https"
    helper.write_text(
        f"#!{sys.executable}\n"
        + textwrap.dedent(
            f"""\
            import os
            import sys

            stdin = sys.stdin.buffer
            stdout = sys.stdout.buffer
            if stdin.readline() != b'capabilities\\n':
                raise SystemExit('expected remote-helper capabilities request')
            stdout.write(b'connect\\n\\n')
            stdout.flush()
            request = stdin.readline()
            services = {{
                b'connect git-upload-pack\\n': {str(upload_pack)!r},
                b'connect git-receive-pack\\n': {str(receive_pack)!r},
            }}
            executable = services.get(request)
            if executable is None:
                raise SystemExit(f'unexpected remote-helper request: {{request!r}}')
            stdout.write(b'\\n')
            stdout.flush()
            environment = dict(os.environ)
            environment.pop('GIT_OBJECT_DIRECTORY', None)
            os.execve(
                executable,
                [executable, os.environ['WRKSLOTS_TEST_HTTPS_REMOTE']],
                environment,
            )
            """
        ),
        encoding="utf-8",
    )
    helper.chmod(0o755)
    monkeypatch.setenv("WRKSLOTS_TEST_HTTPS_REMOTE", str(remote))


def proxy_commands(log: Path) -> list[list[str]]:
    commands: list[list[str]] = []
    for line in log.read_text(encoding="utf-8").splitlines():
        value: object = json.loads(line)
        assert isinstance(value, list)
        command: list[str] = []
        for argument in value:
            assert isinstance(argument, str)
            command.append(argument)
        commands.append(command)
    return commands


def install_credential_helper(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    helper = tmp_path / "real-gh"
    helper.write_text(
        f"#!{sys.executable}\n"
        + textwrap.dedent(
            """\
            import sys

            assert sys.argv[1:] == ['auth', 'git-credential', 'get']
            request = sys.stdin.read()
            assert 'protocol=https' in request
            assert 'host=github.com' in request
            print('username=test-user')
            print('password=test-password')
            """
        ),
        encoding="utf-8",
    )
    helper.chmod(0o755)
    monkeypatch.setenv("FLEET_REAL_GH", str(helper))
    return helper.resolve()


def test_network_git_commands_use_wrapper_and_preserve_git_isolation(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _project, repository, remote = lifecycle.make_project(tmp_path)
    expected = lifecycle.git(repository, "rev-parse", "HEAD").stdout.strip()
    lifecycle.git(repository, "update-ref", "-d", "refs/remotes/origin/main")
    log = install_proxy_wrapper(tmp_path, monkeypatch)
    helper = install_credential_helper(tmp_path, monkeypatch)
    monkeypatch.setenv("GIT_DIR", str(tmp_path / "unrelated-git-directory"))
    monkeypatch.setenv("GIT_CONFIG_COUNT", "1")
    monkeypatch.setenv("GIT_CONFIG_KEY_0", "core.useReplaceRefs")
    monkeypatch.setenv("GIT_CONFIG_VALUE_0", "true")

    vcs = wrkslots._GitVcs()
    authority = vcs.remote_authority(repository, "origin")
    vcs.fetch_remote(repository, "origin", "refs/remotes/origin/main", authority)
    assert vcs.verify_ref(repository, "refs/remotes/origin/main", "fetched ref") == expected
    ref = "refs/salvage/testhost/slot01/network-test"
    vcs.push_salvage(repository, "origin", expected, ref, authority)
    assert vcs.verify_ref(remote, ref, "published ref") == expected
    assert vcs.head(repository) == expected
    assert vcs.status(repository) == ""

    commands = proxy_commands(log)
    assert [command[command.index("-C") + 2] for command in commands] == [
        "ls-remote", "fetch", "ls-remote", "push", "ls-remote"
    ]
    assert all(command[:4] == [
        "git", "--no-replace-objects", "-c", "core.useReplaceRefs=false"
    ] for command in commands)
    expected_helper = f"credential.helper=!{helper} auth git-credential"
    assert all(
        command[4:8]
        == ["-c", "credential.helper=", "-c", expected_helper]
        for command in commands
    )
    fetch = commands[0]
    operation = fetch.index("-C") + 2
    assert fetch[operation:operation + 3] == [
        "ls-remote",
        "--refs",
        "--heads",
    ]
    assert "--no-auto-maintenance" in commands[1]


def test_https_shaped_remote_fetch_and_salvage_use_transport_helper(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _project, repository, remote = lifecycle.make_project(tmp_path)
    publisher = tmp_path / "publisher"
    subprocess.run(
        ["git", "clone", str(remote), str(publisher)],
        check=True,
        capture_output=True,
        text=True,
    )
    lifecycle.git(publisher, "config", "user.name", "Wrkslots Test")
    lifecycle.git(publisher, "config", "user.email", "wrkslots@example.invalid")
    expected = lifecycle.commit_local(
        publisher,
        "https-fetched.txt",
        "remote-helper object transfer\n",
        "https fetched",
    )
    lifecycle.git(publisher, "push", "origin", "main")
    assert (
        lifecycle.git(
            repository,
            "cat-file",
            "-e",
            f"{expected}^{{commit}}",
            check=False,
        ).returncode
        != 0
    )
    log = install_proxy_wrapper(tmp_path, monkeypatch)
    install_https_transport_helper(tmp_path, monkeypatch, remote)
    authorized_url = "https://authority.invalid/repository.git"
    lifecycle.git(repository, "remote", "set-url", "origin", authorized_url)
    lifecycle.git(repository, "update-ref", "-d", "refs/remotes/origin/main")

    vcs = wrkslots._GitVcs()
    authority = vcs.remote_authority(repository, "origin")
    assert authority.url == authorized_url
    vcs.fetch_remote(repository, "origin", "refs/remotes/origin/main", authority)
    assert vcs.verify_ref(repository, "refs/remotes/origin/main", "fetched ref") == expected
    lifecycle.git(repository, "cat-file", "-e", f"{expected}^{{commit}}")

    commit = lifecycle.commit_local(
        repository,
        "https-salvage.txt",
        "transport helper exercised\n",
        "https salvage",
    )
    ref = "refs/salvage/testhost/https-shaped"
    vcs.push_salvage(repository, "origin", commit, ref, authority)
    assert lifecycle.git(remote, "rev-parse", ref).stdout.strip() == commit

    commands = proxy_commands(log)
    assert [command[command.index("-C") + 2] for command in commands] == [
        "ls-remote",
        "fetch",
        "ls-remote",
        "push",
        "ls-remote",
    ]
    assert all(authorized_url in command for command in commands)
    assert all(str(remote) not in command for command in commands)
    assert "--no-auto-maintenance" in commands[1]


def test_fetch_remote_does_not_download_tag_only_history(tmp_path: Path) -> None:
    _project, repository, remote = lifecycle.make_project(tmp_path)
    tag_source = tmp_path / "tag-source"
    subprocess.run(
        ["git", "clone", str(remote), str(tag_source)],
        check=True,
        capture_output=True,
        text=True,
    )
    lifecycle.git(tag_source, "config", "user.name", "Wrkslots Test")
    lifecycle.git(tag_source, "config", "user.email", "wrkslots@example.invalid")
    lifecycle.git(tag_source, "checkout", "--orphan", "tag-only")
    (tag_source / "tag-only.txt").write_text("unrelated\n", encoding="utf-8")
    lifecycle.git(tag_source, "add", "tag-only.txt")
    lifecycle.git(tag_source, "commit", "-m", "tag-only history")
    tag_only = lifecycle.git(tag_source, "rev-parse", "HEAD").stdout.strip()
    lifecycle.git(tag_source, "push", "origin", "HEAD:refs/tags/unreachable")
    assert (
        lifecycle.git(
            repository,
            "cat-file",
            "-e",
            f"{tag_only}^{{commit}}",
            check=False,
        ).returncode
        != 0
    )

    vcs = wrkslots._GitVcs()
    authority = vcs.remote_authority(repository, "origin")
    vcs.fetch_remote(repository, "origin", "refs/remotes/origin/main", authority)

    assert (
        lifecycle.git(
            repository,
            "cat-file",
            "-e",
            f"{tag_only}^{{commit}}",
            check=False,
        ).returncode
        != 0
    )
    assert not vcs.ref_exists(repository, "refs/tags/unreachable")


def test_fetch_remote_refuses_head_change_after_advertisement(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _project, repository, remote = lifecycle.make_project(tmp_path)
    publisher = tmp_path / "moving-publisher"
    subprocess.run(
        ["git", "clone", str(remote), str(publisher)],
        check=True,
        capture_output=True,
        text=True,
    )
    lifecycle.git(publisher, "config", "user.name", "Wrkslots Test")
    lifecycle.git(publisher, "config", "user.email", "wrkslots@example.invalid")
    lifecycle.git(repository, "update-ref", "-d", "refs/remotes/origin/main")
    vcs = wrkslots._GitVcs()
    authority = vcs.remote_authority(repository, "origin")
    original_run = wrkslots._GitVcs._run
    moved = False

    def move_before_fetch(
        repository_path: Path,
        args: Sequence[str],
        *,
        check: bool = True,
        input_text: str | None = None,
        env_overrides: Mapping[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        nonlocal moved
        if args and args[0] == "fetch" and not moved:
            lifecycle.commit_local(
                publisher,
                "moved.txt",
                "changed after advertisement\n",
                "move advertised head",
            )
            lifecycle.git(publisher, "push", "origin", "main")
            moved = True
        return original_run(
            repository_path,
            args,
            check=check,
            input_text=input_text,
            env_overrides=env_overrides,
        )

    monkeypatch.setattr(wrkslots._GitVcs, "_run", staticmethod(move_before_fetch))
    with pytest.raises(wrkslots.Refusal, match="different head inventory"):
        vcs.fetch_remote(repository, "origin", "refs/remotes/origin/main", authority)

    assert moved is True
    assert not vcs.ref_exists(repository, "refs/remotes/origin/main")


def test_salvage_push_preserves_ref_created_after_preflight(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _project, repository, remote = lifecycle.make_project(tmp_path)
    original = lifecycle.git(repository, "rev-parse", "HEAD").stdout.strip()
    commit = lifecycle.commit_local(
        repository,
        "salvage-race.txt",
        "must not overwrite a concurrent ref\n",
        "salvage race",
    )
    ref = "refs/salvage/testhost/concurrent"
    vcs = wrkslots._GitVcs()
    authority = vcs.remote_authority(repository, "origin")
    original_run = wrkslots._GitVcs._run
    created = False

    def create_before_push(
        repository_path: Path,
        args: Sequence[str],
        *,
        check: bool = True,
        input_text: str | None = None,
        env_overrides: Mapping[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        nonlocal created
        if args and args[0] == "push" and not created:
            lifecycle.git(remote, "update-ref", ref, original)
            created = True
        return original_run(
            repository_path,
            args,
            check=check,
            input_text=input_text,
            env_overrides=env_overrides,
        )

    monkeypatch.setattr(wrkslots._GitVcs, "_run", staticmethod(create_before_push))
    with pytest.raises(wrkslots.Refusal, match="Git refused"):
        vcs.push_salvage(repository, "origin", commit, ref, authority)

    assert created is True
    assert lifecycle.git(remote, "rev-parse", ref).stdout.strip() == original


def test_github_credential_helper_works_with_global_git_config_disabled(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    helper = install_credential_helper(tmp_path, monkeypatch)
    arguments = wrkslots._github_credential_helper_args(os.environ)

    completed = subprocess.run(
        ["git", *arguments, "credential", "fill"],
        input="protocol=https\nhost=github.com\n\n",
        text=True,
        capture_output=True,
        check=False,
        env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null"},
    )

    assert completed.returncode == 0, completed.stderr
    assert completed.stdout.splitlines() == [
        "protocol=https",
        "host=github.com",
        "username=test-user",
        "password=test-password",
    ]
    assert arguments == (
        "-c",
        "credential.helper=",
        "-c",
        f"credential.helper=!{helper} auth git-credential",
    )


def test_invalid_configured_github_cli_refuses_before_network(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    missing = tmp_path / "missing-gh"
    monkeypatch.setenv("FLEET_REAL_GH", str(missing))

    with pytest.raises(wrkslots.Refusal, match="configured GitHub CLI is unavailable"):
        wrkslots._github_credential_helper_args(os.environ)


def test_create_hooks_and_recursive_submodules_inherit_wrapper_environment(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    project, repository, _remote = lifecycle.make_project(tmp_path)
    component_head, leaf_head = lifecycle.add_recursive_submodules(
        tmp_path, project, repository
    )
    hook = lifecycle.python_hook(
        "import os, subprocess, sys; from pathlib import Path; "
        "assert os.environ['WRKSLOTS_TEST_PROXY_MARKER'] == 'via-with-proxy'; "
        "subprocess.run([sys.executable, '-c', "
        "\"import os; assert os.environ['WRKSLOTS_TEST_PROXY_MARKER'] == 'via-with-proxy'\"], "
        "check=True); "
        "assert Path('component/leaf/leaf.txt').read_text() == 'leaf sentinel\\n'; "
        "Path('hook-completed').write_text('complete')"
    )
    lifecycle.update_configuration(
        project,
        post_provision_hooks=[
            "git -c protocol.file.allow=always submodule update --init --recursive",
            hook,
        ],
    )
    log = install_proxy_wrapper(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("PATH", f"network-tools{os.pathsep}{os.environ['PATH']}")
    secret = "proxy-secret-must-not-appear"
    monkeypatch.setenv("WRKSLOTS_TEST_PRIVATE_VALUE", secret)

    made = lifecycle.create(project)

    assert made.returncode == 0, made.stderr
    target = lifecycle.checkout(project)
    assert (target / "hook-completed").read_text(encoding="utf-8") == "complete"
    assert lifecycle.git(target / "component", "rev-parse", "HEAD").stdout.strip() == component_head
    assert lifecycle.git(target / "component" / "leaf", "rev-parse", "HEAD").stdout.strip() == leaf_head
    assert len(lifecycle.active_slots(project)) == 1
    commands = proxy_commands(log)
    assert [command[0] for command in commands] == [
        "git",
        "git",
        "/bin/sh",
        "/bin/sh",
    ]
    assert [command[command.index("-C") + 2] for command in commands[:2]] == [
        "ls-remote",
        "fetch",
    ]
    assert secret not in made.stdout + made.stderr + log.read_text(encoding="utf-8")


def test_failed_fetch_wrapper_never_retries_direct_or_registers_slot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    project, _repository, _remote = lifecycle.make_project(tmp_path)
    log = install_proxy_wrapper(tmp_path, monkeypatch)
    monkeypatch.setenv("WRKSLOTS_TEST_PROXY_FAIL", "git")

    refused = lifecycle.create(project)

    assert refused.returncode == 3
    assert "test proxy refused the command" in refused.stderr
    assert lifecycle.active_slots(project) == []
    assert not lifecycle.checkout(project).exists()
    assert not lifecycle.create_journal_path(project).exists()
    commands = proxy_commands(log)
    assert len(commands) == 1
    assert commands[0][commands[0].index("-C") + 2] == "ls-remote"


def test_failed_hook_wrapper_preserves_journal_and_recovery_uses_wrapper(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    hook = lifecycle.python_hook(
        "import os; from pathlib import Path; "
        "assert os.environ['WRKSLOTS_TEST_PROXY_MARKER'] == 'via-with-proxy'; "
        "Path('hook-completed').write_text('complete')"
    )
    project, _repository, _remote = lifecycle.make_project(
        tmp_path, post_provision_hooks=(hook,)
    )
    log = install_proxy_wrapper(tmp_path, monkeypatch)
    monkeypatch.setenv("WRKSLOTS_TEST_PROXY_FAIL", "/bin/sh")

    refused = lifecycle.create(project)

    assert refused.returncode == 3
    assert "test proxy refused the command" in refused.stderr
    assert lifecycle.active_slots(project) == []
    target = lifecycle.checkout(project)
    assert target.is_dir()
    assert not (target / "hook-completed").exists()
    assert lifecycle.create_journal_path(project).is_file()
    assert [command[0] for command in proxy_commands(log)] == [
        "git",
        "git",
        "/bin/sh",
    ]

    monkeypatch.delenv("WRKSLOTS_TEST_PROXY_FAIL")
    recovered = lifecycle.command(
        project, "recover", "--coordinator-pid", str(os.getpid())
    )

    assert recovered.returncode == 0, recovered.stderr
    assert (target / "hook-completed").read_text(encoding="utf-8") == "complete"
    assert len(lifecycle.active_slots(project)) == 1
    assert not lifecycle.create_journal_path(project).exists()
    assert [command[0] for command in proxy_commands(log)] == [
        "git",
        "git",
        "/bin/sh",
        "/bin/sh",
    ]


def test_create_and_remote_round_trip_work_without_installed_wrapper(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    hook = lifecycle.python_hook(
        "import os; from pathlib import Path; "
        "assert os.environ['WRKSLOTS_TEST_NETWORK_ENV'] == 'inherited'; "
        "Path('hook-completed').write_text('complete')"
    )
    project, repository, remote = lifecycle.make_project(
        tmp_path, post_provision_hooks=(hook,)
    )
    tools = tmp_path / "tools-without-proxy"
    tools.mkdir()
    for name in ("git", "python3"):
        executable = shutil.which(name)
        assert executable is not None
        (tools / name).symlink_to(executable)
    monkeypatch.setenv("PATH", str(tools))
    monkeypatch.setenv("WRKSLOTS_TEST_NETWORK_ENV", "inherited")

    made = lifecycle.create(project)

    assert made.returncode == 0, made.stderr
    assert (lifecycle.checkout(project) / "hook-completed").read_text(encoding="utf-8") == "complete"
    assert len(lifecycle.active_slots(project)) == 1
    vcs = wrkslots._GitVcs()
    expected = vcs.head(repository)
    ref = "refs/salvage/testhost/slot01/network-test"
    authority = vcs.remote_authority(repository, "origin")
    vcs.push_salvage(repository, "origin", expected, ref, authority)
    assert vcs.verify_ref(remote, ref, "published ref") == expected

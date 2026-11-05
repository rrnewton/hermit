"""`herdr-run status`: a read-only account of what is in effect.

Non-mutating is the contract, not a description. A status command that started a server or created
a workspace in order to describe it would be reporting on its own side effects, and nobody looking
afterwards could tell -- so the fake below PANICS on every call that would change the session, and
names each one.
"""

from __future__ import annotations

import json
import os

import pytest

from herdr_run.client import Pane
from herdr_run.cli import main
from herdr_run.config import Config
from herdr_run.errors import HerdrUnavailable
from herdr_run.status import (
    Session,
    WorkspaceView,
    inspect_session,
    status_document,
    status_text,
    unreachable,
)


class _ReadOnlyFake:
    """Answers reads and raises on anything that would change the session."""

    def __init__(
        self,
        *,
        running: bool = True,
        workspace: str | None = "w1",
        panes: int = 0,
        tab: bool = False,
    ) -> None:
        self._running = running
        self._workspace = workspace
        self._panes = panes
        self._tab = tab
        self.calls: list[str] = []

    # --- the reads status is allowed to make ---------------------------------------------------

    def preflight(self) -> None:
        self.calls.append("preflight")

    def server_running(self) -> bool:
        self.calls.append("server_running")
        return self._running

    def workspace_id_for_label(self, label: str) -> str | None:
        del label
        self.calls.append("workspace_id_for_label")
        return self._workspace

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        del workspace_id
        self.calls.append("panes")
        return tuple(
            Pane(pane_id=f"w1:p{index}", tab_id="w1:t1", workspace_id="w1")
            for index in range(self._panes)
        )

    def tab_id_for_label(self, workspace_id: str, label: str) -> str | None:
        del workspace_id, label
        self.calls.append("tab_id_for_label")
        return "w1:t1" if self._tab else None

    # --- everything that would change the session ----------------------------------------------

    def ensure_server(self) -> bool:
        raise AssertionError("status must never start a server")

    def create_workspace(self, *, label: str, cwd: str) -> tuple[str, str, str]:
        raise AssertionError("status must never create a workspace")

    def create_tab(self, *, workspace_id: str, label: str, cwd: str) -> str:
        raise AssertionError("status must never create a tab")

    def rename_tab(self, tab_id: str, label: str) -> None:
        raise AssertionError("status must never rename a tab")

    def process_info(self, pane_id: str) -> object:
        raise AssertionError("status must never read foreground process state")

    def read(self, pane_id: str, source: str, lines: int | None = None) -> str:
        raise AssertionError("status must never read pane contents")

    def run(self, pane_id: str, command: str) -> None:
        raise AssertionError("status must never type into a pane")

    def send_keys(self, pane_id: str, keys: str) -> None:
        raise AssertionError("status must never send keys to a pane")


def test_a_stopped_server_is_reported_without_being_started() -> None:
    fake = _ReadOnlyFake(running=False)
    assert inspect_session(fake, Config(), "kvm") == Session("not-running")
    assert fake.calls == ["server_running"]


def test_a_running_server_reports_panes_and_whether_the_tab_exists() -> None:
    fake = _ReadOnlyFake(panes=3, tab=True)
    assert inspect_session(fake, Config(), "kvm") == Session(
        "running", view=WorkspaceView(panes=3, tab_exists=True)
    )
    assert fake.calls == [
        "server_running",
        "workspace_id_for_label",
        "panes",
        "tab_id_for_label",
    ]

    assert inspect_session(_ReadOnlyFake(workspace=None), Config(), "kvm") == Session(
        "running"
    )


def test_a_failing_control_call_is_reported_rather_than_guessed_at() -> None:
    class _FailingWorkspace(_ReadOnlyFake):
        def workspace_id_for_label(self, label: str) -> str | None:
            raise HerdrUnavailable("herdr workspace list timed out")

    session = inspect_session(_FailingWorkspace(), Config(), "kvm")
    assert session == Session("failed", reason="herdr workspace list timed out")
    # A failed lookup must not be folded into "no such workspace": one means the answer is not
    # known, the other means the answer is known and is zero.
    assert "unknown (herdr workspace list timed out)" in status_text(
        Config(), "kvm", "kvm", session
    )


def test_the_report_names_the_configuration_the_policy_and_the_session() -> None:
    config = Config(
        source_path="/project/.herdr-run.yaml", project_root="/project"
    )
    session = Session("running", view=WorkspaceView(panes=3, tab_exists=False))
    text = status_text(config, "kvm", "kvm", session)
    for line in (
        "    file          /project/.herdr-run.yaml\n",
        "    project root  /project\n",
        "    spool dir     .herdr-run\n",
        "    allow         git, gh\n",
        "    prefixes      with-proxy\n",
        "    deny lists    every built-in entry is in force\n",
        "    agent         kvm\n",
        "    workspace     agent-cmds\n",
        "    tab label     kvm (not created yet)\n",
        "    herdr         installed; server is running\n",
        "    panes         3 in workspace \"agent-cmds\"\n",
        "\nNothing was changed: status only reads.\n",
    ):
        assert line in text, f"status omitted {line!r}:\n{text}"

    document = status_document(config, "kvm", "kvm", session)
    assert document["panes"] == {
        "count": 3,
        "detail": '3 in workspace "agent-cmds"',
        "workspace_exists": True,
    }
    assert document["tab"] == {"exists": False, "label": "kvm"}
    assert document["herdr"] == {
        "detail": "installed; server is running",
        "reachable": True,
        "server_running": True,
    }
    assert document["allow_any_program"] is False
    assert document["missing_defaults"] == []


def test_a_deny_list_that_leaves_out_built_in_entries_is_reported() -> None:
    """A file written before an entry joined the built-in defaults does not enforce it; say so."""
    config = Config(
        source_path="/project/.herdr-run.yaml",
        project_root="/project",
        deny_anywhere=("--upload-pack", "--receive-pack"),
        deny_global={"git": ("-c",), "gh": (), "cargo": ()},
    )
    session = Session("not-running")
    assert (
        '    deny lists    leaves out built-in deny_global.git "--config-env", "--exec-path", '
        '"--namespace"; deny_anywhere "--exec"\n'
    ) in status_text(config, "kvm", "kvm", session)
    assert status_document(config, "kvm", "kvm", session)["missing_defaults"] == [
        {"entries": ["--config-env", "--exec-path", "--namespace"], "setting": "deny_global.git"},
        {"entries": ["--exec"], "setting": "deny_anywhere"},
    ]


def test_an_unreachable_herdr_is_said_plainly_and_leaves_the_counts_unknown() -> None:
    session = unreachable("no Herdr executable found")
    text = status_text(Config(), "kvm", "kvm", session)
    assert "    herdr         not reachable — no Herdr executable found\n" in text
    assert "    panes         unknown (herdr is not reachable)\n" in text
    document = status_document(Config(), "kvm", "kvm", session)
    assert document["herdr"] == {
        "detail": "not reachable — no Herdr executable found",
        "reachable": False,
        "server_running": False,
    }
    assert document["panes"] == {
        "count": None,
        "detail": "unknown (herdr is not reachable)",
        "workspace_exists": None,
    }
    assert document["tab"] == {"exists": None, "label": "kvm"}


def test_the_allow_everything_mode_is_reported_as_such_rather_than_as_a_program_named_star() -> None:
    config = Config(allow=("*",), prefixes=())
    text = status_text(config, "kvm", "kvm", Session("not-running"))
    assert '    allow         any program ("*")\n' in text
    assert "    prefixes      (none)\n" in text
    assert status_document(config, "kvm", "kvm", Session("not-running"))[
        "allow_any_program"
    ]


def test_status_reads_and_creates_nothing_on_disk(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """The whole CLI path, with the read-only fake standing in for the session."""
    pytest.importorskip("yaml")
    root = str(tmp_path)
    with open(os.path.join(root, ".herdr-run.yaml"), "w", encoding="utf-8") as handle:
        handle.write("workspace: cross-fixture\nallow: [git, gh]\n")
    monkeypatch.chdir(root)

    import herdr_run.cli as cli_module

    monkeypatch.setattr(cli_module, "_client", lambda *_args: _ReadOnlyFake(panes=2))

    assert main(["--agent", "fixture-agent", "status"]) == 0
    out = capsys.readouterr().out
    assert out.startswith("herdr-run status\n")
    assert "    panes         2 in workspace \"cross-fixture\"\n" in out
    assert "Nothing was changed: status only reads.\n" in out

    assert main(["--json", "--agent", "fixture-agent", "status"]) == 0
    document = json.loads(capsys.readouterr().out)
    assert document["agent"] == "fixture-agent"
    assert document["workspace"] == "cross-fixture"
    assert document["config_file"] == os.path.join(root, ".herdr-run.yaml")

    assert not os.path.exists(os.path.join(root, ".herdr-run")), (
        "status created spool state in a directory it was only supposed to describe"
    )


def test_an_unresolvable_herdr_is_reported_not_raised(
    tmp_path: object,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    """"Herdr is not installed" is one of the most useful things status can say, so it exits 0.

    Resolution is otherwise lazy, and `server_running()` answers False both for "no server" and
    for "no herdr". Reporting the second as the first would tell somebody with nothing installed
    to go looking for a stopped server.
    """

    class _Unresolvable(_ReadOnlyFake):
        def preflight(self) -> None:
            raise HerdrUnavailable("Herdr executable not found in fixed install locations: /x")

        def server_running(self) -> bool:
            raise AssertionError("status must not ask a client it could not even resolve")

    monkeypatch.chdir(str(tmp_path))
    import herdr_run.cli as cli_module

    monkeypatch.setattr(cli_module, "_client", lambda *_args: _Unresolvable())

    assert main(["--agent", "kvm", "status"]) == 0
    out = capsys.readouterr().out
    assert (
        "    herdr         not reachable — Herdr executable not found in fixed "
        "install locations: /x\n" in out
    )
    assert "    panes         unknown (herdr is not reachable)\n" in out

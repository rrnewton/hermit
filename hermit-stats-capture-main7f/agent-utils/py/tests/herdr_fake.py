"""Test-only in-memory stand-in for a live Herdr session.

Models the parts the tool depends on — workspaces and tabs keyed by LABEL, one pane per tab, and a
per-pane foreground-process state — so bring-up, caching, and readiness can be tested without a
Herdr server. ``run`` optionally executes the shell line locally, which means the quoting produced
by :func:`herdr_run.runner.build_shell_command` is exercised by a real shell rather than asserted
against a string.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field

from herdr_run.client import Pane, ProcessInfo
from herdr_run.errors import HerdrUnavailable


@dataclass
class FakeTab:
    """One tab in the in-memory session."""

    tab_id: str
    label: str
    workspace_id: str
    pane_ids: list[str] = field(default_factory=list)


@dataclass
class FakeWorkspace:
    """One workspace in the in-memory session."""

    workspace_id: str
    label: str
    tabs: dict[str, FakeTab] = field(default_factory=dict)


class FakeHerdrClient:
    """Implements the subset of :class:`herdr_run.client.HerdrClient` the tool actually calls."""

    def __init__(
        self,
        *,
        server_running: bool = True,
        execute_locally: bool = True,
        pane_text: str = "[user@host ~]\n$\n",
    ) -> None:
        self.workspaces: dict[str, FakeWorkspace] = {}
        self._server_running = server_running
        self._execute_locally = execute_locally
        self.pane_text = pane_text
        self._next_workspace = 1
        self._next_pane = 1
        #: Tabs ever created per workspace, so an id is never reused after ``close_tab``.
        self._tabs_created: dict[str, int] = {}
        #: Panes reported as running a job rather than sitting at a prompt.
        self.busy_panes: set[str] = set()
        #: Per-pane shell pid, so a sweep can plant a shell that /proc does or does not know.
        self.shell_pids: dict[str, int] = {}
        #: When true, ``panes`` fails the way an unreachable server does. Distinguishing that from
        #: an empty listing is the difference between "reap nothing" and "reap everything".
        self.fail_pane_list = False
        #: When true, ``process_info`` fails the way a busy or restarting server does. A control
        #: call that did not answer says nothing about whether the pane's shell is alive, so the
        #: sweep must reach UNKNOWN and not "the shell is gone".
        self.fail_process_info = False
        #: Tabs for which ``close_tab`` fails the way a refused control call does.
        self.failing_close: set[str] = set()
        #: Panes asked for ``process_info``, in order.
        self.probed: list[str] = []
        #: Tabs closed through ``close_tab``, in order.
        self.closed_tabs: list[str] = []
        #: Every call made, in order — lets a test assert on idempotency rather than infer it.
        self.calls: list[str] = []
        self.commands: list[tuple[str, str]] = []
        self.started_server = False

    # -- server ---------------------------------------------------------------------------------

    def ensure_server(self) -> bool:
        """Pretend to start a server; records whether it had to."""
        self.calls.append("ensure_server")
        if self._server_running:
            return False
        self._server_running = True
        self.started_server = True
        return True

    # -- workspaces / tabs / panes ----------------------------------------------------------------

    def workspace_id_for_label(self, label: str) -> str | None:
        """Resolve a workspace label to its id."""
        self.calls.append(f"workspace_id_for_label({label})")
        for workspace in self.workspaces.values():
            if workspace.label == label:
                return workspace.workspace_id
        return None

    def create_workspace(self, *, label: str, cwd: str) -> tuple[str, str, str]:
        """Create a workspace, including the default tab a real one arrives with."""
        self.calls.append(f"create_workspace({label})")
        workspace_id = f"w{self._next_workspace}"
        self._next_workspace += 1
        pane_id = f"{workspace_id}:p{self._next_pane}"
        self._next_pane += 1
        tab_id = f"{workspace_id}:t1"
        # A real workspace arrives with one default tab labelled "1".
        tab = FakeTab(tab_id=tab_id, label="1", workspace_id=workspace_id, pane_ids=[pane_id])
        self.workspaces[workspace_id] = FakeWorkspace(workspace_id, label, {tab_id: tab})
        return workspace_id, tab_id, pane_id

    def tab_id_for_label(self, workspace_id: str, label: str) -> str | None:
        """Resolve a tab label to its id."""
        self.calls.append(f"tab_id_for_label({workspace_id},{label})")
        workspace = self.workspaces.get(workspace_id)
        if workspace is None:
            raise HerdrUnavailable(f"no such workspace {workspace_id}")
        for tab in workspace.tabs.values():
            if tab.label == label:
                return tab.tab_id
        return None

    def create_tab(self, *, workspace_id: str, label: str, cwd: str) -> str:
        """Create a labelled tab with one pane."""
        self.calls.append(f"create_tab({workspace_id},{label})")
        workspace = self.workspaces[workspace_id]
        created = self._tabs_created.get(workspace_id, len(workspace.tabs)) + 1
        self._tabs_created[workspace_id] = created
        tab_id = f"{workspace_id}:t{created}"
        pane_id = f"{workspace_id}:p{self._next_pane}"
        self._next_pane += 1
        workspace.tabs[tab_id] = FakeTab(tab_id, label, workspace_id, [pane_id])
        return tab_id

    def rename_tab(self, tab_id: str, label: str) -> None:
        """Relabel a tab."""
        self.calls.append(f"rename_tab({tab_id},{label})")
        for workspace in self.workspaces.values():
            if tab_id in workspace.tabs:
                workspace.tabs[tab_id].label = label
                return
        raise HerdrUnavailable(f"no such tab {tab_id}")

    def close_tab(self, tab_id: str) -> None:
        """Close a tab and every pane in it."""
        self.calls.append(f"close_tab({tab_id})")
        if tab_id in self.failing_close:
            raise HerdrUnavailable(f"tab close {tab_id}: herdr refused")
        for workspace in self.workspaces.values():
            if tab_id in workspace.tabs:
                del workspace.tabs[tab_id]
                self.closed_tabs.append(tab_id)
                return
        raise HerdrUnavailable(f"no such tab {tab_id}")

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]:
        """Every pane, optionally restricted to one workspace."""
        if self.fail_pane_list:
            raise HerdrUnavailable("pane list: herdr is not answering")
        out: list[Pane] = []
        for workspace in self.workspaces.values():
            if workspace_id is not None and workspace.workspace_id != workspace_id:
                continue
            for tab in workspace.tabs.values():
                for pane_id in tab.pane_ids:
                    out.append(Pane(pane_id=pane_id, tab_id=tab.tab_id, workspace_id=workspace.workspace_id))
        return tuple(out)

    def pane_exists(self, pane_id: str) -> bool:
        """Is this pane id still present?"""
        return any(pane.pane_id == pane_id for pane in self.panes())

    def process_info(self, pane_id: str) -> ProcessInfo:
        """Report the pane as idle, or as running a job when listed in ``busy_panes``."""
        self.probed.append(pane_id)
        if self.fail_process_info:
            raise HerdrUnavailable("pane process-info: herdr is not answering")
        shell_pid = self.shell_pids.get(pane_id, 100)
        if pane_id in self.busy_panes:
            return ProcessInfo(pane_id=pane_id, shell_pid=shell_pid, foreground_pgid=shell_pid + 100, foreground=((shell_pid + 100, "git", "git push"),))
        return ProcessInfo(pane_id=pane_id, shell_pid=shell_pid, foreground_pgid=shell_pid, foreground=((shell_pid, "bash", "/bin/bash"),))

    def read(self, pane_id: str, *, source: str = "recent-unwrapped", lines: int | None = None) -> str:
        """Return the canned pane text."""
        return self.pane_text

    def run(self, pane_id: str, command: str) -> None:
        """Record the command and, when configured to, execute it through a real shell."""
        self.commands.append((pane_id, command))
        if self._execute_locally:
            # Run through a real shell so the tool's quoting is genuinely exercised.
            subprocess.run(["bash", "-c", command], check=False, capture_output=True)

    def send_keys(self, pane_id: str, keys: str) -> None:
        """Record a key press."""
        self.calls.append(f"send_keys({pane_id},{keys})")

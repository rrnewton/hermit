"""The non-mutating report behind ``herdr-run status``.

``status`` is to this command what ``git status`` is to git: it answers "what would happen if I ran
something here?" and it answers it by looking. It resolves the configuration in effect, states the
policy that configuration produces, and asks the live session what it already contains -- and it
creates nothing along the way. A status command that brought up a server, a workspace, or a tab in
order to describe them would be reporting on its own side effects.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol

from herdr_run.client import Pane
from herdr_run.config import ALLOW_ANY_PROGRAM, Config, describe_missing, missing_defaults
from herdr_run.errors import HerdrRunError
from herdr_run.textfmt import display, quote

__all__ = [
    "Session",
    "WorkspaceView",
    "inspect_session",
    "status_document",
    "status_text",
]


class _ReadOnlyApi(Protocol):
    """The three reads ``status`` is allowed to make, and nothing that changes the session."""

    def server_running(self) -> bool: ...

    def workspace_id_for_label(self, label: str) -> str | None: ...

    def panes(self, workspace_id: str | None = None) -> tuple[Pane, ...]: ...

    def tab_id_for_label(self, workspace_id: str, label: str) -> str | None: ...


@dataclass(frozen=True)
class WorkspaceView:
    """What the running server says about the configured workspace."""

    #: Panes currently in the workspace, which is what ``max_panes`` is measured against.
    panes: int
    #: Whether this agent's tab already exists.
    tab_exists: bool


@dataclass(frozen=True)
class Session:
    """What could be learned about this project's live Herdr session.

    ``state`` is one of ``unreachable`` (Herdr could not be resolved from a trusted install
    location), ``not-running`` (installed, but no server), ``running`` (a server answered), or
    ``failed`` (a server answered but a later control call errored, so the session cannot be
    described). ``running`` with no :attr:`view` means no workspace carries the configured label.
    """

    state: str
    reason: str = ""
    view: WorkspaceView | None = None


def unreachable(reason: str) -> Session:
    """Herdr itself could not be resolved; the reason is the only fact available."""
    return Session("unreachable", reason=reason)


def inspect_session(client: _ReadOnlyApi, config: Config, tab_label: str) -> Session:
    """Ask the live session what it contains. Creates nothing."""
    if not client.server_running():
        return Session("not-running")
    try:
        workspace_id = client.workspace_id_for_label(config.workspace)
        if workspace_id is None:
            return Session("running")
        panes = len(client.panes(workspace_id))
        tab_exists = client.tab_id_for_label(workspace_id, tab_label) is not None
    except HerdrRunError as exc:
        return Session("failed", reason=str(exc))
    return Session("running", view=WorkspaceView(panes=panes, tab_exists=tab_exists))


def _herdr_detail(session: Session) -> str:
    if session.state == "unreachable":
        return f"not reachable — {session.reason}"
    if session.state == "not-running":
        return "installed; no server is running"
    return "installed; server is running"


def _panes_detail(session: Session, workspace: str) -> str:
    if session.state == "unreachable":
        return "unknown (herdr is not reachable)"
    if session.state == "not-running":
        return "unknown (no server is running)"
    if session.state == "failed":
        return f"unknown ({session.reason})"
    if session.view is None:
        return f"no workspace labelled {quote(workspace)} exists yet"
    return f"{session.view.panes} in workspace {quote(workspace)}"


def _tab_detail(session: Session, tab_label: str) -> str:
    shown = display(tab_label)
    if session.state != "running":
        return shown
    if session.view is not None and session.view.tab_exists:
        return f"{shown} (exists)"
    return f"{shown} (not created yet)"


def _allow_detail(config: Config) -> str:
    if config.allows_any_program():
        return f'any program ("{ALLOW_ANY_PROGRAM}")'
    return ", ".join(display(name) for name in config.allow)


def _deny_detail(config: Config) -> str:
    """Whether every built-in deny entry is in force, or which ones the file leaves out."""
    missing = missing_defaults(config)
    if not missing:
        return "every built-in entry is in force"
    return f"leaves out built-in {describe_missing(missing)}"


def _list_or_none(values: tuple[str, ...]) -> str:
    return ", ".join(display(value) for value in values) if values else "(none)"


def status_text(config: Config, agent: str, tab_label: str, session: Session) -> str:
    """Render the human-readable report, in the shape ``git status`` set the expectation for.

    Every value goes through :func:`herdr_run.textfmt.display`, so a path or a name holding a
    terminal control or an invisible character is shown quoted rather than raw.
    """
    source = display(config.source_path) if config.source_path else "(built-in defaults)"
    return (
        "herdr-run status\n"
        "\n"
        "  configuration\n"
        f"    file          {source}\n"
        f"    project root  {display(config.project_root)}\n"
        f"    spool dir     {display(config.spool_dir)}\n"
        "\n"
        "  policy\n"
        f"    allow         {_allow_detail(config)}\n"
        f"    prefixes      {_list_or_none(config.prefixes)}\n"
        f"    deny lists    {_deny_detail(config)}\n"
        "\n"
        "  session\n"
        f"    agent         {display(agent)}\n"
        f"    workspace     {display(config.workspace)}\n"
        f"    tab label     {_tab_detail(session, tab_label)}\n"
        f"    herdr         {_herdr_detail(session)}\n"
        f"    panes         {_panes_detail(session, config.workspace)}\n"
        "\n"
        "Nothing was changed: status only reads.\n"
    )


def status_document(
    config: Config, agent: str, tab_label: str, session: Session
) -> dict[str, object]:
    """Render the machine-readable report."""
    view = session.view if session.state == "running" else None
    workspace_exists = (session.view is not None) if session.state == "running" else None
    return {
        "agent": agent,
        "allow": list(config.allow),
        "allow_any_program": config.allows_any_program(),
        "config_file": config.source_path,
        "missing_defaults": [
            {"entries": list(item.entries), "setting": item.setting}
            for item in missing_defaults(config)
        ],
        "herdr": {
            "detail": _herdr_detail(session),
            "reachable": session.state != "unreachable",
            "server_running": session.state in ("running", "failed"),
        },
        "panes": {
            "count": None if view is None else view.panes,
            "detail": _panes_detail(session, config.workspace),
            "workspace_exists": workspace_exists,
        },
        "prefixes": list(config.prefixes),
        "project_root": config.project_root,
        "spool_dir": config.spool_dir,
        "tab": {
            "exists": None if view is None else view.tab_exists,
            "label": tab_label,
        },
        "workspace": config.workspace,
    }

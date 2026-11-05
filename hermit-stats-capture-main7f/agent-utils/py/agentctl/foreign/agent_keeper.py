#!/usr/bin/env python3
"""Keep one non-agent 'keeper' tab alive in the subagents workspace.

Why this exists
---------------
herdr REFUSES to close the last tab in a workspace
(``cannot close the last tab in a workspace``). Two real failures followed
from that on 2026-08-13:

1. **The split pane.** A launch does: create tab -> start the agent (which
   spawns a pane) -> close the tab's ORIGINAL root pane. When that close is
   refused, the tab is left holding TWO panes side by side. Nobody asked for a
   split; herdr declined to perform the un-split, and the error was swallowed.
2. **A cascading CLI outage.** Retiring the final agent failed on the same
   refusal, and the resulting error made ``agent_status.py`` unusable — a
   cosmetic close broke the tool used to see what is running.

A keeper tab makes the refusal unreachable: while any agent exists there is
always at least one OTHER tab, so closing an agent's tab is always legal.

Lifecycle (the point of keeping it cheap): the keeper exists only while at
least one agent does. ``ensure`` is called when the population goes 0 -> 1;
``retire`` when it returns to 0, so an idle workspace is not left with a
pointless looping shell.

The keeper also earns its space for the human: it re-prints the agent status
table every ``--interval`` seconds, so the workspace shows what is running
instead of a blank pane.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Optional

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib

KEEPER_LABEL: str = "subagents-status"
DEFAULT_INTERVAL_SECONDS: int = 300


def _workspace_id() -> Optional[str]:
    """Resolve the subagents workspace WITHOUT creating one.

    Creating a workspace here would be wrong: if no workspace exists there is
    nothing to keep alive, and conjuring one would leave an empty shell behind.
    """
    try:
        for workspace in lib._herdr_items(lib._herdr("workspace", "list"), "workspaces"):
            if workspace.get("label") == lib.HERDR_WORKSPACE_LABEL:
                wid = workspace.get("workspace_id")
                return wid if isinstance(wid, str) else None
    except Exception:  # noqa: BLE001 - absence is a normal answer, not an error
        return None
    return None


def _keeper_tab_id(workspace_id: str) -> Optional[str]:
    try:
        for tab in lib._herdr_items(lib._herdr("tab", "list", "--workspace", workspace_id), "tabs"):
            if tab.get("label") == KEEPER_LABEL:
                tab_id = tab.get("tab_id")
                return tab_id if isinstance(tab_id, str) else None
    except Exception:  # noqa: BLE001
        return None
    return None


def _agent_count() -> int:
    return len(lib.read_registry())


def ensure(interval: int = DEFAULT_INTERVAL_SECONDS) -> int:
    """Create the keeper tab if the workspace exists and lacks one."""
    wid = _workspace_id()
    if wid is None:
        print("[keeper] no subagents workspace yet — nothing to keep alive")
        return 0
    if _keeper_tab_id(wid) is not None:
        print("[keeper] already present")
        return 0

    here = Path(__file__).resolve().parent
    status = lib._child_python_command(here / "agent_status.py")
    # `|| true` keeps the loop alive when the status call fails; a keeper that
    # dies on a transient error is worse than useless, because its whole job is
    # to still be there.
    loop = f"while true; do {status} || true; sleep {int(interval)}; done"
    created = lib._herdr(
        "tab", "create", "--workspace", wid, "--label", KEEPER_LABEL, "--no-focus"
    )
    tab_id = _keeper_tab_id(wid)
    if tab_id is None:
        print("[keeper] WARNING: created a tab but could not find it by label")
        return 1

    # Text goes to a PANE, not a tab (`herdr pane send-text`). Prefer the pane
    # id the create call reported; fall back to the tab's first pane.
    pane_id: Optional[str] = None
    tab_obj = created.get("tab") if isinstance(created, dict) else None
    if isinstance(tab_obj, dict):
        candidate = tab_obj.get("active_pane_id") or tab_obj.get("pane_id")
        pane_id = candidate if isinstance(candidate, str) else None
    if pane_id is None:
        # `pane list` filters by WORKSPACE (there is no --tab); select the
        # panes belonging to the keeper tab ourselves.
        for pane in lib._herdr_items(
            lib._herdr("pane", "list", "--workspace", wid), "panes"
        ):
            if pane.get("tab_id") != tab_id:
                continue
            candidate = pane.get("pane_id")
            if isinstance(candidate, str):
                pane_id = candidate
                break
    if pane_id is None:
        print(f"[keeper] WARNING: created {tab_id} but found no pane to drive")
        return 1

    # The tab EXISTING is what fixes the bug — it is what makes the last-tab
    # refusal unreachable. The status loop is a convenience for the human, so a
    # failure to start it degrades the view, never the fix. Report it loudly
    # rather than pretending the keeper is fully armed.
    try:
        lib._herdr("pane", "send-text", pane_id, loop + "\n")
        print(f"[keeper] created {tab_id} pane={pane_id} (status every {interval}s)")
    except Exception as exc:  # noqa: BLE001 - visible degradation, not silence
        print(
            f"[keeper] created {tab_id} pane={pane_id}, but could NOT start the "
            f"status loop: {exc}"
        )
        print("[keeper] the keeper still protects tab-close; the loop is cosmetic")
    return 0


def retire(force: bool = False) -> int:
    """Remove the keeper once no agents remain."""
    count = _agent_count()
    if count and not force:
        print(f"[keeper] {count} agent(s) still registered — keeping it")
        return 0
    wid = _workspace_id()
    if wid is None:
        return 0
    tab_id = _keeper_tab_id(wid)
    if tab_id is None:
        print("[keeper] none present")
        return 0
    try:
        lib._herdr("tab", "close", tab_id)
        print(f"[keeper] closed {tab_id}")
    except Exception as exc:  # noqa: BLE001 - loud, never silent
        print(f"[keeper] WARNING: could not close {tab_id}: {exc}")
        return 1
    return 0


def main() -> int:
    """Create, inspect, or retire the workspace status tab."""
    ap = argparse.ArgumentParser(description="Manage the subagents keeper tab")
    ap.add_argument("action", choices=["ensure", "retire", "status"])
    ap.add_argument("--interval", type=int, default=DEFAULT_INTERVAL_SECONDS)
    ap.add_argument("--force", action="store_true", help="retire even if agents remain")
    args = ap.parse_args()

    if args.action == "ensure":
        return ensure(args.interval)
    if args.action == "retire":
        return retire(args.force)

    wid = _workspace_id()
    tab = _keeper_tab_id(wid) if wid else None
    print(f"workspace={wid or '-'} keeper={tab or '-'} agents={_agent_count()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

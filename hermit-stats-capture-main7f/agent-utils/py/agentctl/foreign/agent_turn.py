#!/usr/bin/env python3
"""Run ONE subagent turn to completion, synchronously.

Why this exists
---------------
``agent_send.py`` QUEUES a turn and returns immediately, so the caller learns
nothing about completion. A coordinator then has to remember to poll
``agent_status.py`` — and when it forgets, the agent sits idle holding a task
while everyone believes it is working. That failure was observed three times in
one session (a promotion, a landing, and a toolchain fix all stalled invisibly).

``agent_turn.py`` is the blocking counterpart. It sends the turn and does NOT
return until the runner writes its ``===TURN-DONE <seq> rc=<n>===`` sentinel.
That makes it safe to invoke as a BACKGROUND SHELL COMMAND from a coordinator
whose harness already notifies on background-command exit — turning subagent
completion into a push signal without any new transport.

It also self-heals the presentation: it brings the agent up if it does not
exist, and recreates the herdr tab/tailer if the window has gone away, so a
closed pane degrades the VIEW rather than losing the TURN.

Exit codes
----------
0    turn completed with rc=0
2    turn completed with a non-zero runner rc (agent-level failure)
3    timed out waiting for the sentinel (turn may still be running)
4    operational failure (could not create/heal/send)
"""

from __future__ import annotations

import argparse
import os
import re
import sys
import time
from pathlib import Path
from typing import Optional

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib

SENTINEL_RE_TEMPLATE = r"===TURN-DONE {seq} rc=(?P<rc>[^\s=]+)"
DEFAULT_TIMEOUT_SECONDS = 3600
POLL_INTERVAL_SECONDS = 1.0


def _read_message(args: argparse.Namespace) -> str:
    if args.message_file:
        return Path(args.message_file).read_text()
    if args.message:
        return str(args.message)
    data = sys.stdin.read()
    if not data.strip():
        raise SystemExit("no message: pass text, --message-file, or stdin")
    return data


def _record_for(name: str) -> Optional[lib.AgentRecord]:
    return lib.read_registry().get(name)


def _ensure_agent(name: str, args: argparse.Namespace, message: str) -> Optional[str]:
    """Ensure the agent exists and its presentation is live.

    Returns the transcript path when the brief was consumed as the first turn
    (so the caller waits on seq 0), or None when an explicit send is required.
    """
    rec = _record_for(name)
    if rec is None:
        if not args.cwd:
            raise SystemExit(f"agent {name!r} does not exist; pass --cwd to create it")
        print(f"[agent_turn] creating {name} (mode={args.mode or 'project default'})")
        res = lib.bring_up_agent(
            name,
            cwd=args.cwd,
            brief=message,
            model=args.model,
            harness=args.harness,
            backend=args.backend,
            mode=args.mode,
        )
        # The registry record does NOT carry the transcript path; the UpResult
        # does. Reading it from the wrong object is what broke the first probe.
        return str(res.agent.transcript)

    # Agent exists: heal the VIEW if its window/tab is gone. A dead pane must
    # never be a reason a turn cannot run.
    snap = lib.status_snapshot(name, run_gc=True)
    agents = getattr(snap, "agents", None) or []
    alive = True
    for entry in agents:
        entry_name = entry.get("name") if isinstance(entry, dict) else getattr(entry, "name", None)
        if entry_name != name:
            continue
        if isinstance(entry, dict):
            alive = bool(entry.get("window_alive", True))
        else:
            alive = bool(getattr(entry, "window_alive", True))
    if not alive:
        print(f"[agent_turn] window for {name} is gone — recreating tab/tailer")
        try:
            lib.recreate_window(name)
        except Exception as exc:  # noqa: BLE001 - report, do not mask
            print(f"[agent_turn] WARNING: could not recreate window: {exc}")
    return None


def _wait_for_sentinel(
    transcript: Path, seq: int, timeout: float, *, stream: bool = True,
    start_offset: int = 0,
) -> Optional[str]:
    """Block until the turn's sentinel appears, STREAMING output meanwhile.

    Streaming matters more than it looks: without it the caller's log stays
    empty for the whole turn, so a long-running agent is indistinguishable from
    a hung one — which is the exact confusion this wrapper exists to remove.
    We tail the transcript incrementally and flush, so `tail -f` on the
    background log shows work as it happens.
    """
    pattern = re.compile(SENTINEL_RE_TEMPLATE.format(seq=seq))
    deadline = time.monotonic() + timeout
    # Start at the transcript's END as of dispatch. A context reset restarts
    # sequence numbering, and the transcript RETAINS the old sentinels - so
    # scanning from byte 0 matches a STALE `TURN-DONE <seq>` and reports the
    # turn complete before the agent has run. Observed exactly that.
    offset = start_offset
    pending = ""
    while time.monotonic() < deadline:
        if transcript.exists():
            with transcript.open("r", errors="replace") as handle:
                handle.seek(offset)
                chunk = handle.read()
                offset = handle.tell()
            if chunk:
                if stream:
                    sys.stdout.write(chunk)
                    sys.stdout.flush()
                pending += chunk
                match = pattern.search(pending)
                if match:
                    return match.group("rc")
                # Keep only a tail: the sentinel is one line, so unbounded
                # retention would grow without bound on a long turn.
                if len(pending) > 65536:
                    pending = pending[-4096:]
        time.sleep(POLL_INTERVAL_SECONDS)
    return None


def main() -> int:
    """Submit one headless worker turn and wait for its matching completion marker."""
    ap = argparse.ArgumentParser(description="Run one subagent turn to completion")
    ap.add_argument("name")
    ap.add_argument("message", nargs="?", default=None)
    ap.add_argument("--message-file", default=None)
    ap.add_argument("--cwd", default=None, help="required only when creating the agent")
    ap.add_argument("--model", default=None)
    ap.add_argument("--fresh", action="store_true",
                    help="clear the agent's session first: a NEW TASK gets clean context. "
                         "Costs nothing (every turn re-invokes codex anyway) and avoids "
                         "carrying an unrelated task's context into this one.")
    ap.add_argument("--effort", default=None,
                    help="per-turn reasoning effort; omitted uses SUBAGENT_EFFORT or the harness default")
    ap.add_argument("--harness", default=lib.DEFAULT_HARNESS, choices=list(lib.SUPPORTED_HARNESSES))
    ap.add_argument("--backend", default=None, choices=list(lib.SUPPORTED_BACKENDS))
    ap.add_argument("--mode", default=None, help="headless (default for this path) or tui")
    ap.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    ap.add_argument("--tail", type=int, default=80, help="lines replayed when --quiet")
    ap.add_argument("--quiet", action="store_true", help="suppress live streaming; replay a tail at the end")
    args = ap.parse_args()

    message = _read_message(args)
    # Snapshot the transcript length BEFORE dispatching, so the sentinel search
    # can never match a previous turn's line.
    _pre = _record_for(args.name)
    start_offset = 0
    if _pre is not None:
        try:
            start_offset = lib.transcript_path(args.name).stat().st_size
        except OSError:
            start_offset = 0
    if args.fresh and _record_for(args.name) is not None:
        # Fresh context is the DEFAULT-CORRECT choice at a task boundary. The
        # runner starts turn 1 with `codex exec` and later turns with
        # `codex exec resume <id>`; clearing the session id simply makes the
        # next turn a first turn again. Nothing is destroyed - the tab, the
        # worktree, the transcript and the agent name all persist.
        try:
            res = lib.reset_agent_context(args.name)
            prior = "cleared" if res.previous_session_id else "already empty"
            print(f"[agent_turn] fresh context for {args.name} ({prior})", flush=True)
        except lib.AgentOperationError as exc:
            # Reset requires an idle boundary. A busy agent is a reason to send
            # the turn WITH stale context, never a reason to drop the turn on the
            # floor - dying here silently loses the work the caller asked for.
            print(
                f"[agent_turn] WARNING: could not reset {args.name} ({exc}); "
                "fresh-context turn was not submitted",
                flush=True,
            )
            return 4
    if args.effort:
        # enqueue_message persists this setting for an already-running worker.
        os.environ["SUBAGENT_EFFORT"] = args.effort

    try:
        first_turn_transcript = _ensure_agent(args.name, args, message)
    except lib.AgentOperationError as exc:
        print(f"[agent_turn] FAILED to prepare {args.name}: {exc}")
        return 4

    if _record_for(args.name) is None:
        print(f"[agent_turn] FAILED: {args.name} is not registered after creation")
        return 4

    if first_turn_transcript is not None:
        # The brief WAS the first turn; it is staged as seq 0.
        seq = 0
        transcript = Path(first_turn_transcript)
    else:
        try:
            sent = lib.send_message_to_agent(args.name, message, model=args.model)
        except lib.AgentOperationError as exc:
            print(f"[agent_turn] FAILED to send to {args.name}: {exc}")
            return 4
        seq = sent.seq
        transcript = Path(sent.transcript)
        if sent.quarantined:
            print(f"[agent_turn] WARNING: {len(sent.quarantined)} earlier message(s) quarantined")

    print(f"[agent_turn] {args.name} seq={seq} transcript={transcript}", flush=True)
    rc = _wait_for_sentinel(
        transcript, seq, args.timeout, stream=not args.quiet, start_offset=start_offset
    )

    if rc is None:
        print(f"[agent_turn] TIMEOUT after {args.timeout}s waiting for TURN-DONE {seq}", flush=True)
        return 3

    print(f"[agent_turn] TURN-DONE seq={seq} rc={rc}", flush=True)
    # With streaming on, the output has already been shown; re-printing a tail
    # would just duplicate it. Only replay when streaming was suppressed.
    if args.quiet and transcript.exists() and args.tail > 0:
        lines = transcript.read_text(errors="replace").splitlines()
        print("\n".join(lines[-args.tail :]), flush=True)
    return 0 if rc == "0" else 2


if __name__ == "__main__":
    raise SystemExit(main())

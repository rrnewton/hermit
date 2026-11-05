#!/usr/bin/env python3
"""Queue a message for a running subagent (its next turn).

Non-blocking: the message is appended to the agent's inbox and picked up by the
runner as soon as the current turn (if any) finishes. Errors clearly if the
agent is unknown or its runner is dead.

Usage:
  agent_send.py NAME "message text"   [--model M]
  agent_send.py NAME --message-file F [--model M]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Optional

if __package__ in (None, ""):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from agentctl.foreign import lib


def main() -> int:
    """Read a prompt from text or a file and submit it to the named worker."""
    ap = argparse.ArgumentParser(description="Send a message to a subagent.")
    ap.add_argument("name")
    ap.add_argument("message", nargs="?", default=None, help="message text")
    ap.add_argument("--message-file", default=None, help="file holding the message")
    ap.add_argument("--model", default=None, help="per-turn model override")
    args = ap.parse_args()

    text: Optional[str]
    if args.message_file:
        p = Path(args.message_file)
        if not p.is_file():
            lib.die(f"--message-file not found: {p}")
        text = p.read_text()
    else:
        text = args.message
    if text is None or text == "":
        lib.die("no message: pass text positionally or via --message-file")

    try:
        res = lib.send_message_to_agent(args.name, text, model=args.model)
    except lib.AgentOperationError as exc:
        lib.die(exc.message)
    override = f" (model override: {args.model})" if args.model else ""
    if res.mode == lib.TUI_MODE:
        print(f"delivered queued message {res.seq} to TUI agent {res.name!r}{override}")
        if res.quarantined:
            skipped = ", ".join(str(seq) for seq in res.quarantined)
            print(
                f"WARNING: TUI delivery quarantined failed earlier message(s): {skipped}. "
                f"They are in {lib.failed_dir(res.name)}; inspect or retry deliberately with "
                f"agent_inbox.py {res.name} --inspect.",
                file=sys.stderr,
            )
        print(
            f"  watch: herdr agent read {res.presentation_pane} --source recent-unwrapped --lines 500 "
            "--format text"
        )
    else:
        print(f"queued message as turn {res.seq} for {res.name!r}{override}")
        print(f"  watch: tail -f {res.transcript}  (sentinel: TURN-DONE {res.seq})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

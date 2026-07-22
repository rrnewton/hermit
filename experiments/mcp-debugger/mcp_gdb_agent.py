#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Mock MCP agent for the hermit + GDB-MCP proof of concept.

This is a *deterministic* stand-in for an LLM agent: it speaks the Model
Context Protocol (MCP) to an off-the-shelf GDB MCP server
(pansila/mcp_server_gdb) exactly the way Claude would, but issues a fixed
tool-call sequence instead of asking a model what to do. That makes it
suitable for CI (no tokens, reproducible).

Pipeline exercised:

    hermit run --gdbserver (RSP port on 127.0.0.1)
        ^ target remote
    gdb  <-- driven via GDB/MI by -->  mcp_server_gdb  <-- MCP/stdio -->  THIS AGENT

The agent:
  1. launches `hermit run --gdbserver` on a random loopback port and waits
     until the RSP port is listening (non-destructive readiness poll);
  2. starts the MCP server and completes the MCP initialize handshake;
  3. create_session -> the server's gdb runs a startup command file that
     does `target remote :PORT`, attaching to hermit's stub;
  4. set_breakpoint at the `return sum;` line inside add();
  5. continue_execution -> the breakpoint is hit;
  6. get_stack_frames / get_local_variables -> inspect state;
  7. asserts the agent stopped inside add() with sum == 42.

Exit codes: 0 = PASS, 1 = ran but assertions failed, 2 = environment/setup
problem (treated as SKIP by run_poc.sh).
"""

import argparse
import asyncio
import os
import random
import re
import subprocess
import sys
import tempfile
import time

try:
    from mcp import ClientSession, StdioServerParameters
    from mcp.client.stdio import stdio_client
except ImportError:
    print("[agent] SKIP: python 'mcp' package not installed (run setup.sh)", flush=True)
    sys.exit(2)


def log(*a):
    print("[agent]", *a, flush=True)


def find_break_line(src_path: str) -> int:
    """Locate the line tagged BREAK-HERE in demo.c (avoids hard-coding)."""
    with open(src_path) as f:
        for i, line in enumerate(f, start=1):
            if "@BREAK-HERE" in line:
                return i
    raise RuntimeError(f"no @BREAK-HERE marker in {src_path}")


def port_listening(port: int) -> bool:
    """Non-destructive readiness check. hermit's gdbserver accepts exactly one
    connection, so we must NOT open a probe socket (that would consume it).
    Parse /proc/net/tcp for a LISTEN socket on `port` instead."""
    hexport = f"{port:04X}"
    try:
        with open("/proc/net/tcp") as f:
            next(f)  # header
            for line in f:
                cols = line.split()
                local, state = cols[1], cols[3]
                # state 0A == TCP_LISTEN
                if state == "0A" and local.split(":")[1].upper() == hexport:
                    return True
    except OSError:
        pass
    return False


def wait_listen(port: int, timeout: int = 45) -> bool:
    for _ in range(timeout):
        if port_listening(port):
            return True
        time.sleep(1)
    return False


def text_of(result) -> str:
    return "\n".join(getattr(c, "text", str(c)) for c in result.content)


async def call(session, tool, args, tries=1, delay=0.3):
    """Call a tool, optionally retrying while the server reports it is busy.

    mcp_server_gdb returns from continue_execution as soon as gdb reports
    `^running`; the follow-up `*stopped` async record is processed slightly
    later. Querying in that window yields "GDB busy", so state queries retry."""
    last = ""
    for _ in range(tries):
        res = await session.call_tool(tool, args)
        t = text_of(res)
        if "busy" not in t.lower() and "error" not in t.lower():
            return t
        last = t
        await asyncio.sleep(delay)
    return last


async def run(args) -> int:
    bp_line = find_break_line(args.demo_src)
    port = args.port or random.randint(13000, 13800)
    log(f"launching hermit gdbserver on 127.0.0.1:{port} for {args.demo}")

    # `--network host`: on the pinned reverie the gdbserver otherwise binds
    # inside the guest's isolated net namespace and is unreachable from the
    # host. (hermit PR #144 makes this automatic; passing it explicitly keeps
    # the POC working against plain origin/main too.)
    hermit = subprocess.Popen(
        [args.hermit, "run", "--network", "host", "--gdbserver",
         "--gdbserver-port", str(port), "--", args.demo],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        if not wait_listen(port):
            err = (hermit.stderr.read().decode(errors="replace")
                   if hermit.stderr else "")
            log("SKIP: hermit gdbserver never opened its port")
            if err:
                log("hermit stderr tail:", err[-400:])
            return 2
        log("hermit gdbserver is listening")

        # Startup command file: this is how we get an off-the-shelf server that
        # only knows how to launch a *local* gdb to attach to hermit's *remote*
        # RSP target. gdb runs it before MCP/MI control takes over.
        gi = tempfile.NamedTemporaryFile("w", suffix=".gdbinit", delete=False)
        gi.write("set breakpoint pending on\n")
        gi.write("set sysroot\n")  # skip slow shared-lib transfers over RSP
        gi.write(f"target remote :{port}\n")
        gi.close()

        params = StdioServerParameters(command=args.mcp_gdb, args=[], env=None)
        async with stdio_client(params) as (r, w):
            async with ClientSession(r, w) as s:
                await asyncio.wait_for(s.initialize(), timeout=20)
                log("MCP server initialized")

                cs = await s.call_tool("create_session", {
                    "program": args.demo, "command": gi.name,
                    "nx": True, "quiet": True})
                cs_text = text_of(cs)
                m = re.search(r"([0-9a-f]{8}-[0-9a-f-]{27,})", cs_text)
                if not m:
                    log("SKIP: could not create GDB session:", cs_text[:200])
                    return 2
                sid = m.group(1)
                log("create_session ->", cs_text.strip())

                bp = await s.call_tool("set_breakpoint", {
                    "session_id": sid, "file": os.path.basename(args.demo_src),
                    "line": bp_line})
                log(f"set_breakpoint demo.c:{bp_line} ->", text_of(bp)[:160])

                cont = await s.call_tool("continue_execution", {"session_id": sid})
                log("continue_execution ->", text_of(cont)[:120])

                frames = await call(s, "get_stack_frames", {"session_id": sid},
                                    tries=25)
                log("get_stack_frames ->", frames[:260])
                locs = await call(s, "get_local_variables", {"session_id": sid},
                                  tries=25)
                log("get_local_variables ->", locs[:260])

                ok_frame = '"func":"add"' in frames
                ok_var = '"name":"sum"' in locs and '"value":"42"' in locs
                log(f"ASSERT stopped-in-add={ok_frame}  sum==42={ok_var}")

                # Best-effort teardown; the demo program then runs to completion.
                try:
                    await s.call_tool("continue_execution", {"session_id": sid})
                    await s.call_tool("close_session", {"session_id": sid})
                except Exception as e:  # server quirks on teardown are non-fatal
                    log("(teardown note:", type(e).__name__, str(e)[:60], ")")

                if ok_frame and ok_var:
                    log("PASS: MCP agent set a breakpoint, hit it in add(), "
                        "and read sum==42 over MCP")
                    return 0
                log("FAIL: expected to stop in add() with sum==42")
                return 1
    finally:
        try:
            hermit.terminate()
            hermit.wait(timeout=5)
        except Exception:
            try:
                hermit.kill()
            except Exception:
                pass
        try:
            os.unlink(gi.name)
        except Exception:
            pass


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--hermit", default=os.environ.get("HERMIT_BIN", "hermit"),
                    help="path to the hermit binary")
    ap.add_argument("--mcp-gdb", default=os.environ.get("MCP_GDB_BIN",
                    os.path.join(here, ".build", "mcp_server_gdb", "target",
                                 "release", "mcp-server-gdb")),
                    help="path to the mcp-server-gdb binary")
    ap.add_argument("--demo", default=os.path.join(here, ".build", "demo"),
                    help="path to the compiled demo guest")
    ap.add_argument("--demo-src", default=os.path.join(here, "demo.c"),
                    help="path to demo.c (for line resolution)")
    ap.add_argument("--port", type=int, default=0, help="fixed port (0=random)")
    args = ap.parse_args()

    for label, path in (("hermit", args.hermit), ("mcp-server-gdb", args.mcp_gdb),
                        ("demo", args.demo)):
        if os.path.sep in path and not os.path.exists(path):
            log(f"SKIP: {label} not found at {path} (run setup.sh / build hermit)")
            sys.exit(2)

    sys.exit(asyncio.run(run(args)))


if __name__ == "__main__":
    main()

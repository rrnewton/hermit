#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""LLDB variant of the mock MCP agent: drives stass/lldb-mcp against a hermit
gdbserver. This is the counterpart to mcp_gdb_agent.py.

IMPORTANT: this path only works once the reverie gdbstub LLDB-handshake fix
(rrnewton/reverie PR #21: qHostInfo/qProcessInfo/qRegisterInfo/jThreadsInfo/
QThreadSuffixSupported) is present in the reverie that hermit builds against.
On the currently pinned reverie, LLDB connects but hangs during the handshake,
so this agent will time out and return SKIP (2). See README "LLDB variant" for
the one-line [patch] that makes it work.

Unlike the GDB server, lldb-mcp exposes an arbitrary `lldb_command`, so
attaching to hermit's remote RSP target is a first-class one-liner:
    lldb_command(session, "gdb-remote localhost:PORT")

Exit codes: 0 = PASS, 1 = assertions failed, 2 = SKIP (setup/handshake).
"""

import argparse
import asyncio
import os
import random
import subprocess
import sys
import time

try:
    from mcp import ClientSession, StdioServerParameters
    from mcp.client.stdio import stdio_client
except ImportError:
    print("[lldb-agent] SKIP: python 'mcp' package not installed (run setup.sh)")
    sys.exit(2)


def log(*a):
    print("[lldb-agent]", *a, flush=True)


def port_listening(port: int) -> bool:
    hexport = f"{port:04X}"
    try:
        with open("/proc/net/tcp") as f:
            next(f)
            for line in f:
                cols = line.split()
                if cols[3] == "0A" and cols[1].split(":")[1].upper() == hexport:
                    return True
    except OSError:
        pass
    return False


def wait_listen(port, timeout=45):
    for _ in range(timeout):
        if port_listening(port):
            return True
        time.sleep(1)
    return False


def text_of(result) -> str:
    return "\n".join(getattr(c, "text", str(c)) for c in result.content)


async def run(args) -> int:
    port = args.port or random.randint(13000, 13800)
    log(f"launching hermit gdbserver on 127.0.0.1:{port} for {args.demo}")
    hermit = subprocess.Popen(
        [args.hermit, "run", "--network", "host", "--gdbserver",
         "--gdbserver-port", str(port), "--", args.demo],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        if not wait_listen(port):
            log("SKIP: hermit gdbserver never opened its port")
            return 2
        log("hermit gdbserver is listening")

        params = StdioServerParameters(
            command="python3", args=[args.lldb_mcp], env=None)
        async with stdio_client(params) as (r, w):
            async with ClientSession(r, w) as s:
                await asyncio.wait_for(s.initialize(), timeout=20)
                log("lldb-mcp initialized")

                start = await s.call_tool("lldb_start", {})
                st = text_of(start)
                sid = st.split()[-1].strip().strip(".")
                log("lldb_start ->", st[:120])

                await s.call_tool("lldb_load",
                                  {"session_id": sid, "program": args.demo})
                # First-class remote attach via arbitrary lldb command.
                conn = await asyncio.wait_for(
                    s.call_tool("lldb_command",
                                {"session_id": sid,
                                 "command": f"gdb-remote localhost:{port}"}),
                    timeout=30)
                ctext = text_of(conn)
                log("gdb-remote ->", ctext[:160])
                if "error" in ctext.lower() or "timed out" in ctext.lower():
                    log("SKIP: LLDB could not attach (needs reverie PR #21)")
                    return 2

                await s.call_tool("lldb_set_breakpoint",
                                  {"session_id": sid, "location": "add"})
                await s.call_tool("lldb_continue", {"session_id": sid})
                bt = text_of(await s.call_tool("lldb_backtrace",
                                               {"session_id": sid}))
                log("backtrace ->", bt[:200])
                val = text_of(await s.call_tool(
                    "lldb_print", {"session_id": sid, "expression": "a + b"}))
                log("print a+b ->", val[:120])

                ok = ("add" in bt) and ("42" in val)
                try:
                    await s.call_tool("lldb_continue", {"session_id": sid})
                    await s.call_tool("lldb_terminate", {"session_id": sid})
                except Exception:
                    pass
                if ok:
                    log("PASS: LLDB MCP agent stopped in add() and evaluated a+b==42")
                    return 0
                log("FAIL: expected to stop in add() with a+b==42")
                return 1
    finally:
        try:
            hermit.terminate()
            hermit.wait(timeout=5)
        except Exception:
            pass


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--hermit", default=os.environ.get("HERMIT_BIN", "hermit"))
    ap.add_argument("--lldb-mcp", default=os.path.join(
        here, ".build", "lldb-mcp", "lldb_mcp.py"))
    ap.add_argument("--demo", default=os.path.join(here, ".build", "demo"))
    ap.add_argument("--port", type=int, default=0)
    args = ap.parse_args()

    if not os.path.exists(args.lldb_mcp):
        log(f"SKIP: lldb-mcp not found at {args.lldb_mcp} "
            "(run: WITH_LLDB=1 ./setup.sh)")
        sys.exit(2)
    if os.path.sep in args.hermit and not os.path.exists(args.hermit):
        log(f"SKIP: hermit not found at {args.hermit}")
        sys.exit(2)
    sys.exit(asyncio.run(run(args)))


if __name__ == "__main__":
    main()

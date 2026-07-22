# MCP debugger proof of concept

Drive a hermit debugging session from an **MCP (Model Context Protocol)**
client — i.e. let an AI agent (or a deterministic mock of one) set breakpoints
and inspect program state through an off-the-shelf MCP debugger server.

```
 ┌────────────┐   MCP / stdio    ┌──────────────────┐  GDB/MI   ┌─────┐  RSP / target remote  ┌──────────────────────┐
 │ agent      │ ───────────────▶ │ mcp_server_gdb   │ ────────▶ │ gdb │ ────────────────────▶ │ hermit run --gdbserver│
 │ (or mock)  │                  │ (off-the-shelf)  │           └─────┘        :PORT           │  (guest under detcore)│
 └────────────┘                  └──────────────────┘                                          └──────────────────────┘
```

The mock agent (`mcp_gdb_agent.py`) issues the same MCP tool calls Claude would,
but as a fixed, deterministic sequence — so the pipeline can run in CI with **no
model tokens**.

## What this proves

An agent, speaking only MCP, can:

1. attach a debugger to a program running deterministically under hermit,
2. set a source-level breakpoint,
3. run to it, and
4. read back stack frames and local variables.

Verified locally (see "Result" below): the agent stops inside `add()` and reads
`sum == 42` entirely over MCP.

## Server evaluation: `pansila/mcp_server_gdb` vs `stass/lldb-mcp`

| | **pansila/mcp_server_gdb** (chosen) | **stass/lldb-mcp** |
|---|---|---|
| Language / deps | Rust, single static binary | Python, `pip install mcp` + system `lldb` |
| Debugger | GDB via GDB/MI | LLDB |
| Tools | ~17 (structured: sessions, breakpoints, frames, locals, registers, memory) | ~25 incl. an arbitrary `lldb_command` |
| Remote attach to hermit | via a gdb startup command file (`target remote :PORT`) — no first-class tool | first-class: `lldb_command "gdb-remote :PORT"` |
| Works on hermit **today** | **Yes** — plain GDB breakpoint/continue/inspect works on the pinned reverie | **No** — needs the reverie LLDB-handshake fix (see below) |
| Output shape | JSON strings (easy to assert on) | human-readable LLDB text |
| Rough edges found | `continue` returns before `*stopped` (query retries needed); `get_registers` can drop the connection | needs patched reverie to get past the handshake |

**Decision:** the working POC uses **`mcp_server_gdb`** because plain GDB already
works against the reverie revision hermit pins, so it needs no unmerged
dependencies. `lldb-mcp` is the more ergonomic server (arbitrary commands, clean
remote attach) and is wired up in `lldb_agent.py`, but its hermit path is gated
on the reverie fix landing — see "LLDB variant".

## Why `hermit run --gdbserver`, not `hermit replay --gdbserver-port`

The task framing said "hermit replay → MCP server", but `hermit replay` always
**auto-launches its own `gdb` child** (see `hermit-cli/src/bin/hermit/replay.rs`),
so it cannot hand a bare port to an external MCP-driven client. `hermit run
--gdbserver --gdbserver-port N` is the right primitive: it binds `127.0.0.1:N`
and **blocks in `accept()`** until an external client connects
(`reverie-ptrace .../gdbstub/server.rs`). That is exactly the "expose a port and
wait" behaviour an MCP server needs. Debugging a *recording* the same way is
possible by calling `hermit::replay_with_gdbserver(dir, port)` directly (the CLI
does not expose a wait-only replay port); that is a natural follow-up.

`--network host` is passed explicitly: on the pinned reverie the gdbserver
otherwise binds inside the guest's isolated network namespace and is unreachable
from the host. hermit PR #144 makes this automatic when `--gdbserver` is set;
passing it keeps the POC working against plain `origin/main` too.

## Run it

Prerequisites: `cargo`, `cc`, `gdb`, `python3`, and a host where hermit's
gdbserver runs (PMU access; see the repo README). Network fetches go through the
proxy on devservers.

```bash
cd experiments/mcp-debugger

# 1. build hermit if you have not already (from the repo root)
( cd ../.. && cargo build -p hermit )

# 2. fetch + build the MCP server, make a venv, compile the demo guest
with-proxy ./setup.sh

# 3. run the end-to-end proof of concept
./run_poc.sh
```

`run_poc.sh` exits `0` on PASS, `1` on FAIL, and `2` on SKIP (a missing
prerequisite such as no PMU / no `gdb` / server not built). SKIP keeps CI green
on hosts that cannot run the gdbserver, matching hermit's existing
hardware-sensitivity conventions.

### Result (local run)

```
[agent] hermit gdbserver is listening
[agent] MCP server initialized
[agent] create_session -> Created GDB session: <uuid>
[agent] set_breakpoint demo.c:15 -> Set breakpoint: {... "line":"15" ...}
[agent] continue_execution -> Continued execution: {}
[agent] get_stack_frames -> [{"level":"0","func":"add", ... "line":"15"}, {"level":"1","func":"main", ...}]
[agent] get_local_variables -> [{"name":"a","value":"41"},{"name":"b","value":"1"},{"name":"sum","value":"42"}]
[agent] ASSERT stopped-in-add=True  sum==42=True
[agent] PASS: MCP agent set a breakpoint, hit it in add(), and read sum==42 over MCP
```

## Using a real Claude agent instead of the mock

The same MCP server works with any MCP client (Claude Desktop, Claude Code, the
Agent SDK). Register it and start hermit's gdbserver yourself, then ask the agent
to debug. Example MCP client config:

```json
{
  "mcpServers": {
    "gdb": {
      "command": "experiments/mcp-debugger/.build/mcp_server_gdb/target/release/mcp-server-gdb"
    }
  }
}
```

Then, with `hermit run --network host --gdbserver --gdbserver-port 1234 -- ./demo`
already waiting, prompt the agent: *"Create a GDB session for ./demo whose
startup command file contains `target remote :1234`, break at demo.c:15,
continue, and tell me the value of sum."* The mock agent is just this sequence
frozen in code.

## LLDB variant

`lldb_agent.py` drives `stass/lldb-mcp` over the identical hermit port. LLDB
attaching to hermit needs the reverie gdbstub handshake packets from
**rrnewton/reverie PR #21** (`qHostInfo`/`qProcessInfo`/`qRegisterInfo`/
`jThreadsInfo`/`QThreadSuffixSupported`), which are not in the reverie revision
hermit currently pins. Until that lands, `lldb_agent.py` returns SKIP.

To try it once the fix is available, point hermit's build at a reverie checkout
that contains it, by adding to the repo-root `Cargo.toml`:

```toml
[patch."https://github.com/facebookexperimental/reverie.git"]
reverie        = { path = "/path/to/reverie/reverie" }
reverie-ptrace = { path = "/path/to/reverie/reverie-ptrace" }
# ... plus the other reverie-* members Cargo.lock references
```

then rebuild hermit and run:

```bash
WITH_LLDB=1 with-proxy ./setup.sh
./.build/venv/bin/python lldb_agent.py --hermit ../../target/debug/hermit
```

## Files

| file | purpose |
|---|---|
| `demo.c` | tiny `-g -O0` guest; breakpoint marker `BREAK-HERE` on `return sum;` |
| `setup.sh` | fetch/build `mcp_server_gdb` (pinned), venv + MCP SDK, compile demo |
| `run_poc.sh` | orchestrate the GDB POC; PASS/FAIL/SKIP exit codes |
| `mcp_gdb_agent.py` | deterministic MCP agent (the working POC) |
| `lldb_agent.py` | LLDB/`lldb-mcp` variant (gated on reverie PR #21) |

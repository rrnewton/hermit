# DBI (DynamoRIO) backend — real-app test & performance results

Status: prototype evaluation. Date basis: 2026-07-22.

This documents actually running `hermit run --backend dbi` against real programs
and benchmarking it against the production `ptrace` backend. It follows up on the
DBI wiring (PR #187 wired `--backend dbi`; PR #181 ungated it so it can run).

## Test environment

- Host: AMD EPYC 9D85 (316 hardware threads), Linux `6.13.2-0_fbk13_hardened`.
- DynamoRIO: source build 11.91, installed at `~/dynamorio/install`
  (`DYNAMORIO_HOME=~/dynamorio/install`).
- Hermit: branch `dbi-backend-tests`, based on the PR #181 ungate
  (`eca8962`, = frontier `344200e` + DBI availability fix), `cargo build` debug.
- DBI client: `libreverie_dbi_client.so` built from reverie rev **`69f47d9`**
  ("DBI parity"), i.e. `worktrees/slot12/.../reverie-dbi-native/`.

### ⚠️ Client-revision caveat (reproduced)

The client built from the reverie revision `hermit-cli` currently *pins*
(`e3e2c965`) **SIGSEGVs on dynamic ELFs** (e.g. `/bin/echo`, `/bin/true`) — see
PR #181. All results below therefore use the working `69f47d9` client. Because
`hermit run --backend dbi` only shells out to `drrun -c <client>` (it does not
link `reverie-dbi` in Rust), the client `.so` revision is chosen at client-build
time and is independent of the pinned reverie used by the ptrace/kvm backends.

## How the DBI backend runs (important for interpreting results)

`--backend dbi` (`hermit-cli/src/bin/hermit/backends.rs::run_dbi`) shells out to
DynamoRIO's `drrun` with the `reverie-dbi` client, which runs the guest
**in-process**. The prototype client counts branches (an atomic increment before
every application branch), rewrites `CPUID` to a deterministic identity,
intercepts syscalls without ptrace, forwards `write` through a Reverie tool, and
pins time near epoch 0. **It does not drive Detcore's scheduler.** So the DBI
backend is *not* an apples-to-apples determinism substitute for ptrace: ptrace
runs the full Detcore engine (deterministic scheduling, virtual time, etc.),
while DBI does lightweight in-process instrumentation only. The comparison below
therefore measures the raw cost of each *interception mechanism*, not equivalent
determinism guarantees.

`--strict` has no effect on the DBI path: `run_dbi` bypasses Detcore entirely, so
strict-mode syscall policy is not applied. `hermit run --backend dbi --strict --
<prog>` runs identically to without `--strict`.

## Application compatibility matrix

All commands run as `hermit run --backend dbi -- <cmd>` with the env above.
"Works" = ran to completion with correct output and exit 0.

| App / workload | Result | Notes |
|---|---|---|
| `/bin/echo hello` | ✅ works | prints `hello` |
| `hello` (dynamic C, `write`) | ✅ works | prints `hello world`; `write` forwarded through the Reverie tool |
| `hello` under `--strict` | ✅ works | `--strict` is a no-op on the DBI path |
| `python3 -c 'print(...)'` | ✅ works | Python 3.12; interpreter startup + compute correct |
| `python3` compute loop (1e6) | ✅ works | correct result |
| `curl --version` | ✅ works | curl 7.76.1 |
| `curl file://<local>` | ✅ works | fetched local file contents correctly |
| `redis-server --version` | ✅ works | v6.2.22 |
| `redis-server --test-memory` | ✅ works | full memory-test workload completes |
| `redis-server` (full daemon) | ✅ works | reaches "Ready to accept connections"; served `PING`/`SET`/`GET`/`DBSIZE` via native `redis-cli` and shut down cleanly |

**No hard failures observed** across echo, hello, Python, curl, and Redis
(including a full serving round-trip). The DBI prototype successfully loads and
runs real dynamic ELFs and a real server.

### Behavioral caveats (not crashes, but differences from ptrace)

1. **Time base differs from ptrace / not fully normalized.** `date` under DBI
   prints `Wed Dec 31 04:00:01 PM PST 1969` (≈ epoch 0, local TZ) vs ptrace's
   `Fri Dec 31 2021 UTC`. Redis's startup banner similarly shows
   `01 Jan 1970 -7:-59:-59`. DBI pins time to a constant (so it is deterministic)
   but at a different epoch than Detcore and without timezone normalization.
2. **Determinism is partial.** DBI does not run Detcore's scheduler, so
   multi-threaded scheduling, futex ordering, and most syscall-result
   sanitization are *not* determinized the way ptrace does them. Single-threaded
   CPU output was reproducible run-to-run (the C loop produced identical results
   across repeated runs), but DBI should not be treated as a determinism backend
   yet.

## Performance: native vs ptrace vs DBI

Median wall-clock of 5 runs each, seconds. `pt/nat`, `dbi/nat` = slowdown vs
native; `dbi/pt` = DBI time ÷ ptrace time (<1 means DBI faster).

| workload | native | ptrace | dbi | pt/nat | dbi/nat | dbi/pt |
|---|---:|---:|---:|---:|---:|---:|
| echo (startup) | 0.005 | 0.032 | 0.078 | 6.8× | 16.6× | 2.43 |
| hello (startup) | 0.004 | 0.023 | 0.047 | 5.3× | 11.1× | 2.09 |
| python compute 1e6 | 0.422 | 4.568 | 6.799 | 10.8× | 16.1× | 1.49 |
| C loop 50M (CPU-bound) | 0.022 | 0.275 | 0.408 | 12.4× | 18.4× | 1.48 |
| **sysbench 200k syscalls** | 0.024 | **13.546** | **0.209** | 572× | 8.8× | **0.02** |
| redis --test-memory (CPU) | 1.798 | 3.688 | 9.247 | 2.1× | 5.1× | 2.51 |

Microbenchmark sources are in the appendix.

### Interpretation

The two mechanisms have opposite cost profiles:

- **CPU-bound work → DBI is slower than ptrace** (1.5–2.5×). The prototype
  client instruments *every branch* with an atomic counter, which dominates
  compute-heavy code. ptrace adds almost nothing to a syscall-free hot loop.
- **Syscall-bound work → DBI is dramatically faster than ptrace.** On the
  syscall microbench (200k `getpid` + periodic `write`), ptrace costs **13.5 s**
  (572× native — every syscall round-trips to the out-of-process tracer and
  Detcore) while DBI costs **0.21 s** (8.8× native). DBI is **~64× faster than
  ptrace** here because it intercepts in-process with no context switch.

This is the crossover that matters for the "server apps" motivation: I/O-
multiplexing servers (nginx/Redis/Node) are syscall-bound in their hot path,
which is exactly where in-process DBI interception wins big over ptrace — *if*
the per-branch counting overhead is reduced or made optional. The current
`redis --test-memory` result is CPU-bound (a memory scrubber), so it does not
represent a server's request-serving hot path; a syscall-bound Redis workload
would be expected to favor DBI.

Caveat: ptrace here is doing strictly more work (full Detcore determinism) than
DBI. A fair determinism-for-determinism comparison must wait until DBI drives
Detcore.

## Follow-up bugs / gaps to file

1. **DBI branch-counting overhead makes CPU-bound code 1.5–2.5× slower than
   ptrace.** The atomic per-branch increment should be made optional (only when
   RCB-based preemption is actually needed) or use a cheaper thread-local
   counter. This is the main blocker to DBI's "faster than ptrace" promise.
2. **DBI time base differs from Detcore and TZ is not normalized** (epoch-0 /
   local TZ vs Detcore's 2021 UTC). Align the DBI client's virtual clock with
   Detcore's and normalize timezone.
3. **`e3e2c965`-pinned client crashes on dynamic ELFs** (already tracked in
   PR #181). Bump the client build recipe / pin to a working DBI-line revision.
4. **DBI does not drive Detcore** (known; the prototype is interception-only).
   Full determinism (scheduler, futexes, syscall sanitization) requires wiring
   the client's syscall events into Detcore. Until then `--backend dbi` is a
   fast interceptor, not a determinism backend, and `--strict` is a no-op on it.

## Appendix: microbenchmark sources

CPU-bound (`cbench`):
```c
#include <stdio.h>
int main(void){ volatile unsigned long s=0;
  for(unsigned long i=0;i<50000000UL;i++) s+=i; printf("%lu\n", s); return 0; }
```

Syscall-bound (`sysbench`):
```c
#include <unistd.h>
#include <sys/syscall.h>
#include <fcntl.h>
int main(void){ int fd=open("/dev/null",O_WRONLY);
  for(int i=0;i<200000;i++){ syscall(SYS_getpid); if((i&7)==0) write(fd,"x",1); }
  return 0; }
```

Python compute: `python3 -c 's=0
for i in range(1000000): s+=i'`

Redis serving round-trip:
```bash
hermit run --backend dbi -- /usr/bin/redis-server --port 6417 --save '' --appendonly no &
redis-cli -p 6417 ping        # PONG
redis-cli -p 6417 set k v     # OK
redis-cli -p 6417 get k       # v
redis-cli -p 6417 shutdown nosave
```

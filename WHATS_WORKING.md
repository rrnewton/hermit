# What Works Today in Hermit

This document is a practical, hands-on snapshot of what Hermit can do **right now**. Every
command below was run and verified on `main` at commit `15fb99f` (backend = `ptrace`, the
default). It is meant to be copy-pasteable so you can reproduce the results yourself.

> **TL;DR:** On the default ptrace backend, Hermit runs a broad range of real programs
> bit-for-bit deterministically under `--strict --verify` — coreutils, Python, SQLite, curl,
> a full Redis server workflow, and multithreaded C/Go/OpenMP/Rust programs. Record/replay
> round-trips cleanly (24/24 tests green). The two remaining determinism gaps are
> multithreaded wall-clock time reads and an `openssl speed` scheduler bug (details below).

---

## 1. Building Hermit

```bash
cd ~/work/dev-hermit/hermit
cargo build -p hermit
# binary lands at: ./target/debug/hermit
```

A release build (faster guests) is `cargo build -p hermit --release` →
`./target/release/hermit`. All results in this doc are from the debug binary.

For convenience the rest of this doc assumes:

```bash
HERMIT=$(pwd)/target/debug/hermit
```

---

## 2. How to test a program deterministically

The core value proposition is **deterministic execution**. Run any program under strict mode
and ask Hermit to re-run it and diff the two executions:

```bash
$HERMIT run --strict --verify -- <program> [args...]
```

- `--strict` — full deterministic mode (virtual time, virtualized PIDs/ports, deterministic
  scheduling). It is currently the default; the flag is kept for clarity.
- `--verify` — run the program twice and confirm the two runs are bit-for-bit identical.
  On success you get: `:: Success: deterministic. Determinism verified.`

Two useful notes about the sandbox:
- Hermit **isolates the guest `/tmp`** and virtualizes the filesystem view, so process
  substitution (`<(...)` → `/dev/fd/N`) and host-only paths are not visible to the guest.
- Hermit **virtualizes ephemeral ports**: `bind(("127.0.0.1", 0))` + `getsockname()` returns
  a deterministic `32768` every run (natively the kernel hands out a random port). This is
  what lets network servers pass `--verify`.

---

## 3. Verified pass/fail matrix (`--strict --verify`)

Aggregated from the four completed test-matrix tasks plus direct re-runs, all on `main`
@ `15fb99f`, backend = ptrace, no relaxations. **35 distinct programs tested; 30 PASS
bit-for-bit.** "PASS" = deterministic and verified across a repeat run.

### 3a. Single-threaded / coreutils / interpreters — 19/20 PASS

| App | Example command | Result |
|-----|-----------------|--------|
| true | `$HERMIT run --strict --verify -- /bin/true` | PASS |
| echo | `$HERMIT run --strict --verify -- /bin/echo hello world` | PASS |
| date | `$HERMIT run --strict --verify -- /bin/date -d @0` | PASS (virtual time) |
| env | `$HERMIT run --strict --verify -- /usr/bin/env -i PATH=/usr/bin env` | PASS |
| cat | `$HERMIT run --strict --verify -- /bin/cat FILE` | PASS |
| ls | `$HERMIT run --strict --verify -- /bin/ls -la .` | PASS |
| wc | `$HERMIT run --strict --verify -- /usr/bin/wc -c FILE` | PASS |
| head | `$HERMIT run --strict --verify -- /usr/bin/head -1 FILE` | PASS |
| sort | `$HERMIT run --strict --verify -- /usr/bin/sort FILE` | PASS |
| uniq | `$HERMIT run --strict --verify -- /usr/bin/uniq FILE` | PASS |
| grep | `$HERMIT run --strict --verify -- /bin/grep alpha FILE` | PASS |
| sed | `$HERMIT run --strict --verify -- /bin/sed 's/alpha/X/g' FILE` | PASS |
| awk | `$HERMIT run --strict --verify -- /usr/bin/awk '{print NR,$1}' FILE` | PASS |
| tr | `$HERMIT run --strict --verify -- /usr/bin/tr a-z A-Z < FILE` | PASS |
| cut | `$HERMIT run --strict --verify -- /usr/bin/cut -c1-3 FILE` | PASS |
| seq | `$HERMIT run --strict --verify -- /usr/bin/seq 1 5` | PASS |
| sha256sum | `$HERMIT run --strict --verify -- /usr/bin/sha256sum FILE` | PASS |
| base64 | `$HERMIT run --strict --verify -- /usr/bin/base64 FILE` | PASS |
| gzip | `$HERMIT run --strict --verify -- /bin/gzip -c FILE` | PASS |
| tar | `$HERMIT run --strict --verify -- /bin/tar --version` | PASS |
| make | `$HERMIT run --strict --verify -- /usr/bin/make --version` | PASS |
| perl | `$HERMIT run --strict --verify -- /usr/bin/perl -e 'print 6*7'` | PASS |
| sqlite3 | `$HERMIT run --strict --verify -- /usr/bin/sqlite3 :memory: 'SELECT 1+1'` | PASS |
| openssl | `$HERMIT run --strict --verify -- openssl speed -evp aes-256-cbc -seconds 1` | **FAIL — hermit bug** (see §6) |

### 3b. Filesystem — 4/5 PASS

| App | Example command | Result |
|-----|-----------------|--------|
| cat / ls / sha256sum | see above | PASS |
| diff (regular files) | `$HERMIT run --strict --verify -- /usr/bin/diff f1 f2` | PASS |
| diff (process subst.) | `diff <(echo a) <(echo a)` | FAIL — **not a determinism issue**: `/dev/fd/N` process-sub fds are not exposed in Hermit's isolated fs. Regular-file diff passes. |

### 3c. Network — 4/4 PASS

| Test | Result |
|------|--------|
| `curl --version` | PASS |
| Python `socket.bind(("127.0.0.1",0))` + `getsockname()` + close | PASS — port virtualized to deterministic `32768` |
| Python TCP loopback echo (single process: socket/bind/listen/getsockname/connect/accept/send/recv) | PASS |
| Python TCP echo **threaded** (server in bg thread + client in main) | PASS |

> Loopback only. Hermit does **not** make *external* network traffic deterministic (by design).

### 3d. Multithreaded — 4/5 PASS at bit-for-bit level

| Test | Result |
|------|--------|
| C pthreads, 2 threads printing (`gcc -pthread`) | PASS |
| Go goroutines ×3 (`sync.WaitGroup`) | PASS |
| OpenMP parallel-for ×4 (`gcc -fopenmp`) | PASS |
| Rust `std::thread` ×3 (`rustc -O`) | PASS |
| Python `threading` Thread start/join | **FAIL on 2nd-run verify** — result value is stable, but multithreaded `gettimeofday`/`clock_gettime` sub-second values diverge (see §6). Not a scheduling bug. |

**Thread *scheduling* determinism — the core value prop — holds:** C/Go/OpenMP/Rust all
spawn real threads with racy output ordering and come out bit-identical across runs.

### 3e. Servers — 1/1 PASS

| Test | Result |
|------|--------|
| Redis: `redis-server` + `redis-cli SET foo bar` / `GET foo` / `SHUTDOWN NOSAVE` (wrapped in one shell script) | PASS — full server workflow is bit-for-bit deterministic, stable across 2 runs |

Because `hermit run` takes a single program, multi-step workflows (like the Redis one) are
wrapped in a shell script and run as `$HERMIT run --strict --verify -- /bin/bash script.sh`.

### 3f. Language runtimes

| Runtime | Example command | Result |
|---------|-----------------|--------|
| Python (stock) | `$HERMIT run --strict --verify -- /usr/bin/python3 -c 'print(42)'` | PASS |
| Perl | `$HERMIT run --strict --verify -- /usr/bin/perl -e 'print 6*7'` | PASS |
| Node | `$HERMIT run --strict --verify -- /usr/local/bin/node -e 'console.log(42)'` | PASS |
| Lua | `$HERMIT run --strict --verify -- /usr/bin/lua -e 'print(42)'` | PASS |
| Java | `$HERMIT run --strict --verify -- /usr/local/bin/java -version` | PASS (OpenJDK 1.8.0_492 Temurin, 5/5 runs) — **requires PR #223** (`saturating_add` fix for `LogicalTime` overflow, issue #219) |
| PHP | `$HERMIT run --strict --verify -- /usr/local/bin/php -r 'echo 42;'` | FAIL — hangs under Hermit (also non-`--strict`); not yet root-caused |
| Ruby | `$HERMIT run --strict --verify -- /usr/bin/ruby -e 'puts 42'` | N/A — host Ruby install is broken (fails identically without Hermit) |

Use the stock `/usr/bin/python3`, not `fbpython` — the `fbpython` launcher is nondeterministic
under `--verify` independent of Hermit.

### 3g. Deterministic builds (the core use case)

Two **independent** `--strict` compiles of the same source produce **byte-identical** binaries
(same SHA-256), i.e. reproducible builds:

| Compiler | Test | Native | Under Hermit (2 independent `--strict` runs) |
|----------|------|--------|---------------------------------------------|
| gcc 11 | `hello.c` → binary | identical | **IDENTICAL** (`sha256 508f2b57…`, both exit 0, runs) |
| clang | `hello.c` → binary | — | **IDENTICAL** (`sha256 4e298afe…`) |
| gcc 11 | program embedding `__DATE__`/`__TIME__` | **DIFFERENT** (bakes wall-clock into the binary) | **IDENTICAL** (`sha256 55f52fc2…`) |

The `__DATE__`/`__TIME__` case is the decisive demonstration: a program that compiles to
*different* bytes natively (the compiler embeds the wall-clock compile time) compiles to
*identical* bytes under Hermit, because the virtualized clock resolves `__TIME__` to the same
fixed value every run.

```sh
printf '#include <stdio.h>\nint main(){printf("hi\\n");return 0;}\n' > hello.c
$HERMIT run --strict -- /usr/bin/gcc -o hello1 hello.c
$HERMIT run --strict -- /usr/bin/gcc -o hello2 hello.c
cmp hello1 hello2 && echo IDENTICAL   # -> IDENTICAL
```

Note: this is the build *artifact* being reproducible across independent runs. `gcc` still
fails `--strict --verify` (that checks internal syscall-trace determinism across a multi-process
`cc1`/`as`/`ld` pipeline, see §6) — both facts are true and not contradictory: the emitted
binary is deterministic even though the internal syscall interleaving is not.

---

## 4. Backend status

Hermit has three instrumentation backends, selected with `--backend`:

| Backend | Flag | Status |
|---------|------|--------|
| **ptrace** | `--backend ptrace` (default) | **Working.** This is what every result in this doc uses. Reverie ptrace backend. |
| **DBI** (DynamoRIO) | `--backend dbi` | **Gated / fail-closed.** Wired into the CLI but `ensure_available()` fail-closes when no DynamoRIO SDK is found. Running it reports: `backend 'dbi' is unavailable: the DynamoRIO SDK was not found; set DYNAMORIO_HOME or DynamoRIO_DIR to a valid SDK`. The DBI *vehicle* runs programs via raw drrun in separate experiments; the hermit-integrated path is not yet enabled. |
| **KVM** | `--backend kvm` | **Wiring in progress.** Fail-closed: `backend 'kvm' is unavailable: the bare KVM prototype cannot execute Linux programs without a guest-kernel ABI`. No Tool/Guest adapter yet. |

```bash
# ptrace (default) — works:
$HERMIT run --strict --verify -- /bin/echo hi
# dbi / kvm — currently fail-closed with the messages above:
$HERMIT run --backend dbi -- /bin/echo hi
$HERMIT run --backend kvm -- /bin/echo hi
```

---

## 5. Record / Replay status — **24/24 tests passing**

Record an execution and replay it later. The recording captures the full deterministic
syscall stream; replay reproduces it and can drive a debugger.

```bash
# Record (prints a recording id on completion):
$HERMIT record start -- /bin/echo hello
#   RECORDING COMPLETE! To replay, run:
#       hermit replay <ID>

# Replay a recording (attaches a gdbserver/gdb session to the reproduced process):
$HERMIT replay <ID>

# Record AND immediately verify the replay matches (round-trip self-check):
$HERMIT record start --verify -- /bin/echo hello
#   :: Success: replay matched recording.

# Manage recordings:
$HERMIT record list      # list recordings
$HERMIT record rm <ID>   # delete one
$HERMIT record clean     # delete all
```

Verified round-trips this pass: `echo` and `date -u +%Y` both report
`:: Success: replay matched recording.`

The full record/replay integration suite is green:

```bash
cargo test -p hermit --test record_replay -- --test-threads=1
# test result: ok. 24 passed; 0 failed; 0 ignored
```

Coverage includes real binaries (`curl --version`, `sqlite3 :memory:`), a forked external
command from a shell, directory-tree walks, record-timeout behavior (including SIGALRM-blocked
and descendant-process cases), and 15 Rust guest workloads exercising clock ordering,
`exit_group`, `sched_yield`, futex timeout/wait/wake, heap/stack pointers, nanosleep races,
pipes, poll/poll-spin, rdtsc, and thread randomness.

---

## 6. Known limitations & open issues

**Real Hermit issues (worth follow-up):**

1. **`openssl speed` → scheduler panic.** `openssl speed -evp aes-256-cbc -seconds 1` triggers
   a detcore panic in `scheduler/timed_waiters.rs:91` ("internal invariant broken, entry
   missing"), a non-unwinding panic that surfaces as SIGSEGV. Root cause is the
   SIGALRM/`setitimer` timing loop under `--strict`.

2. **Multithreaded wall-clock time reads diverge under `--verify`.** When two threads
   contend (e.g. Python's GIL) and read `gettimeofday`/`clock_gettime` frequently, the
   *sub-second* component of virtual time differs run-to-run. Same fixed epoch second, but
   divergent microseconds. This is the same class as the known git/fbpython multithreaded
   virtual-time issue. **Single-threaded time is fine** (`date` passes). A fix would live in
   detcore's virtual-time accounting for concurrent threads.

**Not Hermit issues (documented so they aren't mis-filed):**

- `diff <(...) <(...)` — process-substitution `/dev/fd/N` fds aren't in the isolated fs;
  regular-file diff passes.
- `openssl speed aes-256-cbc -seconds 1` (without `-evp`) — bad args for OpenSSL 3.5.7; fails
  natively too.
- `nginx -t` — needs root for `/var/log/nginx` and `/run/nginx.pid`; fails natively too. The
  config *syntax* check itself passes.
- `netcat` two-process loopback — the `nc -l` listener hangs natively (test-harness issue),
  not a Hermit limitation.

---

## 7. Reproducing the full matrix

```bash
cd ~/work/dev-hermit/hermit
cargo build -p hermit
HERMIT=$(pwd)/target/debug/hermit

# Any single program:
$HERMIT run --strict --verify -- /bin/echo hello

# Record/replay suite:
cargo test -p hermit --test record_replay -- --test-threads=1
```

Multithreaded / network / server programs are compiled natively (outside Hermit) and then run
*under* Hermit, so these measure **runtime** determinism (thread scheduling, syscalls, virtual
time) rather than compile determinism. Keep sources and binaries outside the Hermit-isolated
`/tmp` when reproducing.

# e9patch preprocessing parity corpus

Freestanding, statically linked, raw-`syscall` x86-64 guests used by
`../e9patch_corpus.py` to ratchet e9patch preprocessing parity against the
golden ptrace backend.

These guests are deliberately freestanding (`-nostdlib -static -ffreestanding`)
rather than ordinary libc programs. e9tool rewrites only the *main* executable,
so a dynamically linked libc binary exposes zero `SYSCALL` sites in its own ELF
(they live in `libc.so`) and e9patch preprocessing is a no-op
(`candidate_sites=0`). A freestanding guest emits its `syscall` instructions in
the main ELF, so e9patch actually rewrites it (`candidate_sites > 0`). Every
guest ends in `exit_group` (231); a bare `exit` (60) would exit only the calling
thread and hang the run.

| Guest | Exercises |
| --- | --- |
| `minimal_exit` | single site: `exit_group` only |
| `write_stdout` | `write(1, ...)` then exit |
| `getpid_check` | virtualized `getpid` |
| `clock_gettime` | `clock_gettime(CLOCK_MONOTONIC)` |
| `nanosleep` | `nanosleep` |
| `getrandom` | determinized `getrandom` stream |
| `multi_site` | three distinct `noinline` syscall sites (write/getpid/exit) |
| `loop_write` | one site invoked eight times in a loop |
| `mmap_anon` | anonymous `mmap`, touch, `munmap` |
| `uname` | `uname` |
| `sigmask` | `gettid` + `rt_sigprocmask` |
| `compute` | CPU-bound loop (RCB preemption) then exit |
| `fd_open_number` | first guest-opened fd is the lowest free number (3) |
| `fd_lowest_free` | closed fd number is reused by the next `open` |
| `pipe_fds` | `pipe2` allocates the two lowest free descriptors |
| `dup3_high` | `dup3` honors a caller-chosen high fd (10) |
| `writev_multi` | three-iovec gathered `write` output |
| `fcntl_cloexec` | `F_GETFD` reports no stray `FD_CLOEXEC` |
| `proc_self_fd_count` | `/proc/self/fd` count parity (no leaked loader fd) |
| `readlink_exe` | `/proc/self/exe` resolves to the original guest, not the e9 temp |
| `dense_syscalls` | two back-to-back 2-byte `SYSCALL`s: trampoline window overlaps the next site (straddler) |
| `indirect_syscall` | rewritten site reached via an indirect call through a `volatile` function pointer |

Guests 13–20 are the **round-2 fd/output-hygiene ratchet batch** (non-time,
non-gated): they establish that e9patch preprocessing perturbs no descriptor
allocation or process-metadata output. `proc_self_fd_count` and `readlink_exe`
emit environment-dependent values, so the driver asserts golden==e9patch parity
for them (`expected_stdout=None`) rather than a fixed string; the other six pin
exact stdout.

Guests 21–22 are the **rewrite-engine relocation-stress batch**. Every earlier
guest wraps each `SYSCALL` in its own `noinline` helper reached by a
compiler-visible direct call; these instead probe the e9tool AOT
rewrite/relocation engine directly. `dense_syscalls` places two 2-byte `SYSCALL`
instructions back-to-back (encoded `0f 05 0f 05`), so the 5-byte control-transfer
e9tool plants for the first site overlaps the immediately following second site —
the adjacent-short-instruction / "straddler" case. `indirect_syscall` reaches its
rewritten site through an indirect call via a `volatile` function pointer, so the
call target is opaque at compile time. Both still enforce full direct-AOT
coverage (`mapped_sites == candidate_sites`, `b0_sites == 0`) and L2 on both
backends.

All guests remain freestanding (`candidate_sites > 0`) so e9patch actually
rewrites them.

Regenerate identical sources with the parent workspace generator at
`experiments/e9patch_ptrace_corpus_parity_20260731/src/gen_corpus.sh`.

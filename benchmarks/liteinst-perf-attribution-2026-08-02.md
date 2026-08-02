# LiteInst syscall fast-path attribution

## Result

The old LiteInst host hybrid was not a patch fast path. Its patched trampoline
raised `SIGTRAP` for every syscall, then the ptrace host validated the marker,
read `/proc/<pid>/maps`, and ran the Tool. At 100,000 calls it did not complete
within 45 seconds (and a separate run was interrupted after 194 seconds).

Hermit `c89eaddb0aa39a44ef7daeb2959af085ea418351` instead uses Reverie
`7401ed949d61a2f30614f332fad7d185a499fcfc`: the Tool runs in the guest, only
`GlobalTool` operations use the coordinator socket, and Hermit's currently
single-threaded constructor explicitly selects quiescent patch publication.
The generic Reverie entry point remains concurrent-safe.

Five measured strict-mode samples after one warmup gave these medians:

| Mode | 100,000 calls | Relative to ptrace |
| --- | ---: | ---: |
| Native | 16.302 ms | 257.22x faster |
| Ptrace | 4,193.171 ms | 1.00x |
| LiteInst in-guest/quiescent | 1,416.672 ms | 2.96x faster |
| LiteInst host hybrid | >45,000 ms (timeout) | >10.73x slower |

The in-guest path reduces wall time by 66.2% versus ptrace and is more than
31.8x faster than the timed-out host hybrid. Quiescent publication itself is a
first-hit optimization: on matched 100,000-call `perf stat -r 5` runs,
concurrent and quiescent in-guest task-clock were 1,975.48 ms and 1,974.26 ms.
The large speedup comes from removing the per-syscall ptrace round trip.

## Attribution

The 10,000-call counter run separates fixed startup from steady-state work:

| Mode | Task-clock | Instructions | Context switches | Page faults |
| --- | ---: | ---: | ---: | ---: |
| Ptrace | 506.84 ms | 1.483 billion | 73,187 | 1,430 |
| LiteInst host hybrid | 1,002.12 ms | 7.242 billion | 62,477 | 28,300 |
| LiteInst in-guest | 266.90 ms | 0.853 billion | 39,939 | 30,803 |

`perf record` put the legacy samples in the `int3`/ptrace stop path and in
`reverie_ptrace::task::guest_maps` plus its `/proc/<pid>/maps` parser. That is
the marker validation performed by `classify_liteinst_trap` for every patched
syscall. The in-guest profile instead showed Unix-stream send/receive and
scheduler waits below `forward_nested_tool_syscall`.

`strace -f -c` on in-guest runs from 1 to 1,000 workload calls added about three
`sendto`, five `recvfrom`, and two `epoll_wait` operations per additional
intercepted syscall. Coordinator RPC is therefore the remaining steady-state
cost. Counts at 1 and 1,000 calls were:

| Syscall | 1 call | 1,000 calls |
| --- | ---: | ---: |
| `sendto` | 196 | 3,196 |
| `recvfrom` | 328 | 5,327 |
| `epoll_wait` | 145 | 2,143 |

Runs with 1, 10, and 100 calls stayed between 70 and 100 ms on the in-guest
path. This bounds constructor plus first-hit patch installation as fixed cost;
it cannot produce the old workload-proportional slowdown. Runtime activation
also reported `traps=1, hooks=32`, confirming that the measured site left the
trap path and repeatedly used its installed hook.

## Fast-path boundary

The publication choice is attached to the in-process dispatcher:

- `install_tool` and `install_tool_from_bootstrap` use concurrent publication,
  including the calibrated straddler protocol.
- `install_tool_quiescent` is an explicit unsafe caller assertion and skips
  concurrent tearing/straddler protection.
- Hermit's Detcore preload selects the quiescent entry point because the current
  LiteInst backend rejects application-created threads and Hermit permits at
  most one guest thread to execute. Thread lifecycle support must switch this
  constructor back to concurrent publication unless it preserves that invariant.

This change does not complete the flagship lifecycle work. Thread clone,
`clone3`, `vfork`, `exec`, vDSO interception, unpatchable-site ptrace fallback,
PMU preemption, and root-outlives-descendants supervision remain outside the
validated boundary.

## Reproduction

The measured host was Linux `6.18.39-0_fbk0_hardened_0_ga43d5727b443` on an
AMD EPYC 9D85, with `perf_event_paranoid=1`. Load average at completion was
60.33/72.47/86.24, so absolute times should not be treated as idle-host
microbenchmarks. Ratios use alternating backend order in the same run.

```sh
with-proxy cargo build --release --locked -p hermit --bin hermit
with-proxy scripts/stage-liteinst-runtime.sh release \
  target/release/libreverie_liteinst.so \
  target/liteinst-runtime-build-7401ed94
with-proxy ./benchmarks/targeted.py --skip-build \
  --hermit target/release/hermit \
  --backends native,ptrace,liteinst \
  --benchmarks syscall_heavy --iterations 5 --warmups 1
```

The fixture retains its 100,000-call default and now accepts an optional count
for startup/slope measurements, for example
`target/hermit-targeted-benchmarks/syscall-heavy 1000`.

`perf trace` itself was unavailable because this host denies the required BPF
program and raw tracepoint access. The attribution instead used `perf stat`,
`perf record`/`perf report`, and `strace -f -c`; no determinism relaxations were
used. A strict `--verify` run of 1,000 calls also passed with 37/37 total and
Detcore messages and no substantive log differences.

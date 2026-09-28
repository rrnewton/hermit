# PR529 + PR538 fixed-base 27-cell audit

## Immutable run identity

- Hermit measurement commit: `5c0bae832d515ce2b8b9cfcf2d7b97f010d641c6`
- Hermit measurement tree: `b32b3a432c5f3346b0e02c91a0cf983170e49618`
- Hermit product-source parent: `9d4bb692ddfe02241e6341a2faeb83782215e1a5`
  (tree `ce0e6e0203206b2b412919168082e92a9a4e71b9`)
- Reverie integration commit: `d0decf28738521b9ae0a33c4b930c5f3f3d43d27`
- Reverie integration tree: `209cf49e90bb83787865e300fb3c56f3add1813d`
- Ordered Reverie parents: PR529 `c632c111619cb47922a72235b3e1130b91355603`,
  then approved PR538 repair `90ad5b98fa897f03e817d74b1fa66e68f1b758fb`
- Compiled Hermit binary SHA-256:
  `0dfd148b3a4ce05ccc2e1bdb087aece0f4c76c2683136d5d61998bfaed96507f`
- Pinned image: `localhost/hermit-hermetic-validate@sha256:e38c3b2d5cd8a17ed2f99b4a24dd76c9ee63329cdc8c9723e4650ab085fae985`
- Guest libc: glibc 2.42
- Machine/kernel: `devbig014`, `7.1.3-0_fbk0_rc18_0_gd373cd4b8dbf`
- Launcher SHA-256:
  `6f171f49cb4615732bdb653da276ca62c803c31439448a9ec17329a248a02b7e`
- Reviewed 27-cell identity SHA-256:
  `2b2e885fac606893172f716f06489c32e98622c44099db4ee0a16df4fe70aa75`

The completed run is immutable under:

`/home/newton/work/dev-hermit/worktrees/slots/integration-pr529-pr538-diagnostic/integration-evidence/brs-pr529-pr538-90ad-r2-canonical-1900877-1789013353477743636`

Canonical service evidence:

- unit: `validate-kvm-pr529-pr538-5c0bae-20260910T040836Z.service`
- record: `/home/newton/work/dev-hermit/ignored/validate/runs/validate-kvm-pr529-pr538-5c0bae-20260910T040836Z.json`
- log: `/home/newton/work/dev-hermit/ignored/validate/validate-kvm-pr529-pr538-5c0bae-20260910T040836Z.log`
- state/result/exit: `completed` / `passed` / `0`
- elapsed: 2026-09-10 04:09:18Z through 04:19:53Z; validate-lock
  measured 634.187 seconds
- qgroup after the run: 2,409,185,280 referenced bytes, 670,916,608
  exclusive bytes, below the 12,884,901,888-byte hard limit
- all validation-lock slots were free after release

Artifact hashes:

- `summary.json`: `8d997dece79623d5e4b5e3422f8eb7435ff203dadbb1860ba878f5c4148ffff4`
- `invocations.tsv`: `70dc5455edb96684facf2b49a3d7800e76d24c8149fc581591cae2564a2e9ba2`
- `all-results.jsonl`: `b8c691d290d09a91a2585501684d37155658bccdc26e42adcdd757f4ea2bcf45`
- `completion.txt`: `5745794e6ae3611db677842c51bcf318e4fbea3f0631190d88b6dda5f3e4b849`
- canonical record: `361ef855fcb3ac99238454cc366210e32af78bbafdf84820e6e3b42ea568b340`
- canonical service log: `d305561d94fa9819a14a0981a33996d21e349865fde3c52db7707a2c0b691ae0`

The source guard restored `ci/manifest-plan/src/runner.rs`: its live hash and
saved-original hash are both
`53b4a31aabc6e3270637a8e4551cbb8fb455537c0f8c90360582bc8ae8d060c6`;
the compiled one-attempt mutation is separately preserved at hash
`b86624d21f577ce4d01f8aa6bd2770f805bd76ac68cb27e9fb4fbc583de947da`.
Tracked status after the run is exactly ` M Cargo.lock`. No cell was retried.

## Result

The evidence is structurally complete: 27 expected cells, 27 distinct observed
cells, 27 invocations, 27 result rows, zero retry rows, one nested attempt per
row, and no relaxations. Outcomes are 11 PASS, 11 deterministic FAIL, and five
typed timeouts (three CPU and two wall).

PASS:

- `c-programs/just-spin`
- `c-programs/printf-with-threads`
- `c-programs/prodcons-determinism`
- `c-programs/random-sources`
- `c-programs/sigmask-preemption`
- `c-programs/sysinfo-uptime`
- `determinism-stress-c/lock-free`
- `determinism-stress-c/pid-tid`
- `determinism-stress-c/producer-consumer`
- `determinism-stress/order-violation`
- `language-runtimes/ruby-random`

All 16 nonpasses, grouped by their first divergent record or timeout location:

| Cell | Result | First location | Run 1 | Run 2 |
| --- | --- | ---: | --- | --- |
| `backend-parity-c/pthread-lifecycle` | determinism failure | record 188 | inbound `futex(...,265,6,...)` | inbound `futex(...,265,7,...)` |
| `c-programs/get-robust-list-thread` | determinism failure | record 140 | inbound `fstat(1,...)` | inbound `futex(...,265,4,...)` |
| `c-programs/nanosleep-threads-nocrash` | determinism failure | record 146 | inbound `futex(...,265,4,...)` | inbound `exit_group(0)` |
| `c-programs/sched-yield-progress` | determinism failure | record 616 | completed `futex(...,265,4,...)=Ok(0)` | immediate-fizzle `futex` because `0 != 4` |
| `c-programs/sigtimedwait-timeout-0s` | determinism failure | record 722 | immediate-fizzle `futex` because `0 != 4` | completed `futex(...,265,4,...)=Ok(0)` |
| `c-programs/sigtimedwait-timeout-1s` | determinism failure | record 4703 | inbound `futex(...,265,4,...)` | inbound `exit_group(0)` |
| `c-programs/thread-sync-determinism` | determinism failure | record 729 | completed `futex(...,265,4,...)=Ok(0)` | immediate-fizzle `futex` because `0 != 4` |
| `c-programs/threadexhaustion` | determinism failure | record 251 | inbound `futex(...,265,8,...)` | inbound `munmap(...)` |
| `chaos-c/lock-granularity` | determinism failure | record 691 | inbound `futex(...,265,5,...)` | inbound `futex(...,265,4,...)` |
| `data-handling/zstd-multithread` | determinism failure | record 27326 | internal-I/O noncommit poll for `read` | completed `read(...,4096)=Ok(0)` |
| `determinism-stress-c/pid-tid-identity` | determinism failure | record 168 | inbound `rt_sigprocmask(...)` | inbound `futex(...,265,4,...)` |
| `c-programs/sigtimedwait-no-timeout` | CPU timeout | run 1, 22.349 s | SIGTERM after CPU bound | no run 2 |
| `c-programs/writev-determinism` | CPU timeout | run 1, 22.867 s | SIGTERM after CPU bound | no run 2 |
| `determinism-stress-c/signal-order` | CPU timeout | run 1, 22.345 s | SIGTERM after CPU bound | no run 2 |
| `determinism-stress/thread-contention` | wall timeout | run 1, 57.177 s | SIGTERM after wall bound | no run 2 |
| `language-runtimes/node-v8-jit` | wall timeout | run 1, 57.177 s | SIGTERM after wall bound | no run 2 |

Three failures reproduce the exact fixture's scheduler/futex mechanism:
`sched-yield-progress` and `thread-sync-determinism` have successful futex
completion in run 1 versus the immediate-fizzle branch in run 2, while
`sigtimedwait-timeout-0s` has the same pair in the opposite direction. Other
failures are separately identified above; 11 cells pass.

## Comparison provenance

There is no completed PR529-only 27-cell run. The intended PR529-only run is
documented at
`/home/newton/work/dev-hermit/ignored/qualify-kvm-pr529-diagnostic-status-20260909.md`:
it failed before execution because its run record was absent, ran 0/27 cells,
and produced no result directory. It therefore cannot support a PR538-only
causal claim.

The only earlier evidence with these exact 27 identities is
`/home/newton/work/dev-hermit/worktrees/slots/kvm-ratchet-90-run8/ignored/kvm-ratchet-90-qualification-9d4bb692-run13/evidence`.
Its selected identity set C-sorts to the same SHA-256 `2b2e885f...`, but it is a
pre-PR529 baseline: Hermit `9d4bb692ddfe02241e6341a2faeb83782215e1a5`,
Reverie `8c8c0a57649c9ffbf8a7a14291a64320f64b935f` (tree
`ab7212467f3c4150f3147329cd5bcc2ecc8413c9`), binary SHA-256
`3a7c463feb07a520e72e8e5aa2ef79a374d2d3c139c92e2c3a86d3a6dd828c55`.
All 54 selected rows (27 cells, attempts 1 and 2) ended in the same
`previous guard is live` wall-timeout mechanism. Relative to that baseline,
the combined arm changes all 27 away from that timeout: 11 to PASS, 11 to
determinism failure, three to CPU timeout, and two to wall timeout. Because
both PR529 and PR538 differ from that baseline, this delta is not attributable
to PR538 alone. Prepared PR529-only and PR538-only arms are required for that
causal split and must not be conflated with a prior run that never executed.

## Current-main selection check

Read-only current Hermit main was
`8a6a3e996d63655bb4cacba54c4dd4a4c9e9f561` (tree
`c01126068b2bf17963f229edb10a6ab151044c4b`); its
`ci/compat-envelope/cells.json` SHA-256 was
`fb30e447c07407c98929d25c6f7ebab5316a898a7b8dfbb6d82043350dc3ba47`.
Only five of the fixed 27 are currently enabled for KVM verify:
`pthread-lifecycle`, `just-spin`, `prodcons-determinism`,
`thread-sync-determinism`, and `ruby-random`. In this fixed-old-base combined
run, the middle three pass and `pthread-lifecycle` plus
`thread-sync-determinism` diverge. This is not canonical current-main
validation and authorizes no selection change.

## Socket message-state integration audit

The approved PR538 code is correct for host aggregate vectored-I/O semantics,
but the separate in-progress message-state branch adds state transitions that
the mechanical composition must preserve. Read-only inspection used
`/home/newton/work/dev-hermit/worktrees/slots/kvm-inotify` at
`611cb6b018fc29a7a0421eb264970c768cd2bda0` and its uncommitted integration
checkout `/home/newton/work/dev-hermit/worktrees/slots/kvm-inotify-vectored`.

The relevant message-state types are `SocketDescriptionState`,
`PendingSocketMessages`, `PendingSocketMessage`, `PendingDescriptorRight`,
`PendingSocketRight`, and `SocketMessageDestination`. Scalar `write` holds the
per-description send lock, publishes one zero-rights message for the complete
payload before the host operation, and rolls back or finishes the marker after
the host result. Scalar `read` holds `SOCKET_MESSAGE_IO_LOCK` and consumes the
corresponding queued metadata after the host read.

The first uncommitted composition omitted those transitions in `vectored_io`:

- socket `writev` and current-position `pwritev2(offset=-1)` invoked the single
  aggregate host write without the send lock or publish/rollback/finish steps;
- socket `readv` and current-position `preadv2(offset=-1)` invoked the host read
  without the message-I/O lock or pending-message consumption;
- datagram read EFAULT is side-effecting in the measured PR538 contract (the
  host discards the packet), so its queued message marker must also be consumed;
  stream read EFAULT is non-consuming and must retain the marker;
- non-current positioned calls still fail through native seekability ordering
  and must not mutate message state.

Without those hooks, plain vectored payloads can shift stream SCM_RIGHTS byte
boundaries and leave datagram markers stale for later `recvmsg`. This finding
was sent to the owner of `kvm-inotify-vectored`; that owner independently
confirmed it and is implementing the send/read accounting. No edits were made
to that worktree here. The active signal-phase-one branch does not intentionally
change socket/message representation, so there is no additional overlap there.

## Preserved failed infrastructure run

The first launcher attempt is preserved unchanged as unit
`validate-kvm-pr529-pr538-5c0bae-20260910T040053Z.service`, record
`/home/newton/work/dev-hermit/ignored/validate/runs/validate-kvm-pr529-pr538-5c0bae-20260910T040053Z.json`,
log `/home/newton/work/dev-hermit/ignored/validate/validate-kvm-pr529-pr538-5c0bae-20260910T040053Z.log`,
and result directory
`integration-evidence/brs-pr529-pr538-90ad-1168374-1789012321389076699`.
It ran zero cells and exited 2 because the service's `en_US` collation sorted
the same 27-member set differently from the stored C-ordered file. The repaired
launcher exports `LC_ALL=C` and separately verifies membership and exact order.

Nothing was pushed or merged, no assertion was weakened, and no result was
retried.

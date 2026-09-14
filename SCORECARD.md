# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means this manifest cell is selected by full in `ci/expected-e2e-plan.json`; ordinary validation therefore requires it to pass. **Red** means the cell is in the manifest but is not selected by full. **Red does not mean failed:** a red cell may have passed, failed, produced no verdict, or never run. Manifest-disabled combinations are **Not applicable**; they are neither red nor omitted. The current generated data counts Green as **730** and Red as **144**. The generator classifies the current **4526** manifest-disabled combinations as **Not applicable**.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 347 | 18 | 715 | 1080 |
| `dbt` | 0 | 61 | 1019 | 1080 |
| `kvm` | 243 | 8 | 829 | 1080 |
| `sabre` | 112 | 32 | 936 | 1080 |
| `liteinst` | 28 | 25 | 1027 | 1080 |
| **Total** | **730** | **144** | **4526** | **5400** |

## Denominator, and why the percentage is not comparable across changes to it

Green is **730 of 5400**, which is **13.52%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`
- modes: `chaos`, `replay`, `verify`

⚠️ **4526 of those 5400 cells are NOT APPLICABLE** — their backend is not enabled for their mode, so they were never asked to run and cannot pass or fail. Over the 874 cells that CAN run, green is **83.52%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 730 green cells measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly red RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend. The summary columns use the same Green, Red, and Not applicable statuses as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 341 / 360 | 0 / 360 | 243 / 360 | 112 / 360 | 28 / 360 | 724 | 142 | 934 | 1800 |
| `replay` | 1 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 1 | 0 | 1799 | 1800 |
| `chaos` | 5 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 0 / 360 | 5 | 2 | 1793 | 1800 |
| **Total** | | | | | | **730** | **144** | **4526** | **5400** |

## Native controls

Native naked execution is retained as a separate control and is not a Hermit backend. Every control remains visible below, but none contributes to the backend denominator, Green/Red/Not applicable totals, mode totals, or the Status and measurement cross-tab. Canonical `stress-series/v4` retains its ordered attempt and diversity evidence.

| Lane | Category | Test | Mode | Backend | Status |
| --- | --- | --- | --- | --- | --- |
| `portable` | `applications` | `applications/c-toolchain-workflow` | `naked` | `native` | `not-applicable` |
| `portable` | `applications` | `applications/example-timed-progress-bar` | `naked` | `native` | `not-applicable` |
| `portable` | `applications` | `applications/git-repository-workflow` | `naked` | `native` | `not-applicable` |
| `portable` | `applications` | `applications/timed-progress-bar` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/aio-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/append-pwrite` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/bind-getsockname` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/cachestat-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/child-subreaper-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/close-range-fds` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/copy-file-range-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/cpu-virtualization` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/cwd-roundtrip` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/dup-shared-offset` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/environment-and-workdir` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/epoll-pwait2` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/epoll-readiness` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/event-delivery-ordering` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/eventfd-semantics` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/faccessat2-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fadvise-hints` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fallocate-extents` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fchmod-bits` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fchmodat2-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fcntl-owner` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fd-duplication` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/file-backed-mmap` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/file-io-roundtrip` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/flock-lifecycle` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fork-exec-pipeline` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/fsync-durability` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/ftruncate-sparse` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/getcpu-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/getpriority-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/getrusage-self-accounting` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/hardware-trap-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/host-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/inline-syscall-sites` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/inotify-watch` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/ioctl-fionread` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/kcmp-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/linkat-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/lseek-positioning` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mce-kill-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/membarrier-query` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/memfd-create` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mempolicy-default` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mincore-residency` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mixed-inline-and-libc-syscalls` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mkdir-rmdir` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mknod-special` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/mmap-layout-pointer-order` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/msync-writeback` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/name-to-handle-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/no-new-privs-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/numa-node-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/o-tmpfile-anon` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/openat-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/openat2-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/path-file-ops` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/personality-domain` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pid-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pidfd-open-self` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pipe-capacity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pipe-capacity-pin` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pipe-ipc` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pipe-multiwriter-ordering` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pipe2-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/poll-readiness` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/prctl-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/prctl-pdeathsig` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/preadv2-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/pthread-lifecycle` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/readdir-entries` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/readdir-order-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/record-lock` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/rename-ops` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/renameat2-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/rlimit-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/robust-list` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/sched-getaffinity-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/seccomp-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/sendfile-copy` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/set-tid-address` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/short-io-split-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/shutdown-socketpair` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/signal-delivery-sequence` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/signal-waitstatus-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/signalfd-create` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/socket-epoll-ordering` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/socket-options` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/socketpair-flags` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/sockname-unnamed` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/stat-metadata-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/statfs-free-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/static-nolibc-syscall-sites` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/statx-metadata` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/symlink-ops` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/sync-file-range` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/sysv-ipc-refusal` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/thp-disable` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/timer-family-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/umask-mode` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/uname-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/utimensat-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/vectored-file-io` | `naked` | `native` | `not-applicable` |
| `portable` | `backend-parity-c` | `backend-parity-c/vectored-io` | `naked` | `native` | `not-applicable` |
| `portable` | `bin-c` | `bin-c/posix-timer-test` | `naked` | `native` | `not-applicable` |
| `portable` | `bin-c` | `bin-c/robust-futex-test` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/acct-refusal-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/add-key-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/adjtimex-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/arch-prctl-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/bpf-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/cachestat-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/clock-adjtime-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/clone` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/copy-file-range-refusal-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-copied-tiocgpgrp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-exec-failure` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-execveat-unsupported` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-mmap-exec` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-pid-virtualization` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-prlimit-self` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-self-sigqueue` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-unsupported-syscall` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/dbt-wait-lifecycle` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/epoll-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/fp-reduction-nondeterminism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/futex-requeue-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/futex-waitv-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/futex-wake-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/get-robust-list-child` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/get-robust-list-self` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/get-robust-list-thread` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/getcpu` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/getitimer-determinism-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/getsockopt-null` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/hello-alarm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/hello-nostdlib` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/hello-signals` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/io-uring-fallback` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/io-uring-ring-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ioctl-fioclex` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ioctl-siocethtool` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ipc-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/just-spin` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/kcmp-eperm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/keyctl-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/keyctl-passthrough` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/listmount-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/liteinst-advanced` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/lsm-get-self-attr-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/lsm-list-modules-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/lsm-set-self-attr-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/madvise-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/map-shadow-stack-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/memfd-secret-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/meminfo-available-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/meminfo-cached-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/meminfo-free-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/memorypress` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/mmap-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/mmap-stress-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/name-to-handle-at-eopnotsupp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/name-to-handle-directory-eopnotsupp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/name-to-handle-empty-path-eopnotsupp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/name-to-handle-regular-eopnotsupp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/nanosleep-par` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/nanosleep-threads-nocrash` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/nanosleep-threads-simple` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netlink-autobind-generic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netlink-autobind-route` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netlink-autobind-usersock` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netns-cookie-tcp4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netns-cookie-tcp6` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/netns-cookie-udp4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pause-alarm-interrupt` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/perf-event-hardware-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/perf-event-open-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/perf-event-software-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/perf-event-watchpoint-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/periodic-setitimer-delivery` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pidfd-open-self` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pidfd-poll-self` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pidfd-waitid-child` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pipe2-errno-precedence` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ppoll-readv` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ppoll-simulation` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/prctl-dumpable` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/prctl-option-policy` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pread64-nostdlib` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/print-memaddrs` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/printf-with-threads` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/proc-fd-link-aliases` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/proc-fdinfo` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/proc-locks` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/process-mrelease-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/process-vm-readv-refusal-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/process-vm-writev-refusal-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/procfs-identity-agreement` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/procfs-positioned-probe` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/prodcons-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pselect6-simulation` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ptrace-attach-eperm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ptrace-eperm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ptrace-seize-eperm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ptrace-traceme-eperm` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/pty-nr-count` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/racewrite-nostdlib` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/random-sources` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/rcx-canonicalization` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/record-replay-fd-close` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/record-replay-file-state` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/record-replay-file-state-regular-sink` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/record-replay-lseek-seek-cur` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/record-replay-setsockopt` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/recvmsg-scm-rights-mmap` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/remap-file-pages-anonymous-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/remap-file-pages-memfd-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/remap-file-pages-tmpfile-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/request-key-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/resource-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sched-setattr-batch` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sched-setattr-idle` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sched-setattr-other` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sched-yield-progress` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/scheduler-policy-queries` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/session-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/setitimer-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sigmask-preemption` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/signal-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sigpipe-siginfo` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sigtimedwait-no-timeout` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sigtimedwait-timeout-0s` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sigtimedwait-timeout-1s` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/so-incoming-cpu-tcp4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/so-incoming-cpu-tcp6` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/so-incoming-cpu-udp4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-cookie-tcp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-cookie-udp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-cookie-unix` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-ioctl-timestamp` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-timestamp-edge-cases` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-timestamp-timespec` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/socket-timestamp-timeval` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/splice-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/statmount-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/syscall-file-io` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/syscall-file-metadata` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/syscall-quick-wins` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sysfs-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sysinfo` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sysinfo-uptime` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/syslog-deterministic` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sysv-sem-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/sysv-shm-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/tcp-info-accept4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/tcp-info-accept6` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/tcp-info-client4` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/tee-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/thread-self-procfs-handoff` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/thread-sync-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/threadexhaustion` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/timer-create-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/uname` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/unix-autobind-dgram` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/unix-autobind-seqpacket` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/unix-autobind-stream` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/ustat-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/vforkexec` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/vmsplice-enosys` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/wait-on-child` | `naked` | `native` | `not-applicable` |
| `portable` | `c-programs` | `c-programs/writev-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `chaos-c` | `chaos-c/lock-granularity` | `naked` | `native` | `not-applicable` |
| `portable` | `data-handling` | `data-handling/archive-roundtrip` | `naked` | `native` | `not-applicable` |
| `portable` | `data-handling` | `data-handling/dd-partial-transfers` | `naked` | `native` | `not-applicable` |
| `portable` | `data-handling` | `data-handling/jq-json-transform` | `naked` | `native` | `red` |
| `portable` | `data-handling` | `data-handling/shell-pipeline` | `naked` | `native` | `not-applicable` |
| `portable` | `data-handling` | `data-handling/sqlite-query-determinism` | `naked` | `native` | `red` |
| `portable` | `data-handling` | `data-handling/zstd-multithread` | `naked` | `native` | `not-applicable` |
| `portable` | `debugger-c` | `debugger-c/debuggee` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress` | `determinism-stress/example-race` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress` | `determinism-stress/order-violation` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress` | `determinism-stress/process-chains` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress` | `determinism-stress/thread-contention` | `naked` | `native` | `red` |
| `portable` | `determinism-stress` | `determinism-stress/thread-interleaving` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress` | `determinism-stress/thread-output` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/fork-tree` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/lock-free` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/mmap-fork-shared` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/pid-tid` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/pid-tid-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/pipe-chain` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/pipe-prefill` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/producer-consumer` | `naked` | `native` | `red` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/signal-order` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/thread-contention` | `naked` | `native` | `not-applicable` |
| `portable` | `determinism-stress-c` | `determinism-stress-c/thread-stress` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/bash-loop-pipe-time` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/bash-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/cpp-stl-determinism` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/example-python-random` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/gawk-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/lua-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/m4-macro-mkstemp` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/node-v8-jit` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/perl-hash-order` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/perl-io-subprocess-time` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/perl-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/python-dict-hash-iteration` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/python-hash-determinism` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/python-hashseed` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/python-io-subprocess-time` | `naked` | `native` | `not-applicable` |
| `portable` | `language-runtimes` | `language-runtimes/python-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/ruby-random` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/rust-hashmap-iteration` | `naked` | `native` | `red` |
| `portable` | `language-runtimes` | `language-runtimes/tcl-rand-clock` | `naked` | `native` | `red` |
| `portable` | `shared-futex-c` | `shared-futex-c/qemu-exec-init` | `naked` | `native` | `not-applicable` |
| `portable` | `shared-futex-c` | `shared-futex-c/qemu-hello` | `naked` | `native` | `not-applicable` |
| `portable` | `shared-futex-c` | `shared-futex-c/qemu-init` | `naked` | `native` | `not-applicable` |
| `portable` | `shared-futex-c` | `shared-futex-c/qemu-net-init` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/auxv-loader-dump` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/clock-exec-continuity` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/date-nanoseconds` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/du-tree-summary` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/errno-path-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/example-date` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/example-devrand` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/file-timestamp-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/find-tree-metadata` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/harness-width-contract` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/mcookie-random` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/mktemp-name` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/nscd-neutralised` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/openssl-enc` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/openssl-genpkey` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/openssl-passwd` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/openssl-rand` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/openssl-x509` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/overflow-gid-resolves` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/proc-random-uuid` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/proc-uptime` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/procfs-sanitized-paths` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/ps-proc-table` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/random-device` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/record-getpid` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/shm-coherency-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/shuf-permutation` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/sort-random` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/ssh-keygen-ed25519` | `naked` | `native` | `red` |
| `portable` | `system-utils` | `system-utils/startup-surface-identity` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/startup-tls-guards` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/sysfs-sanitized-prefixes` | `naked` | `native` | `not-applicable` |
| `portable` | `system-utils` | `system-utils/uuidgen-random` | `naked` | `native` | `red` |
| `portable` | `util-c` | `util-c/pmu-skid` | `naked` | `native` | `not-applicable` |
| `privileged` | `applications` | `applications/kvm-python-examples` | `naked` | `native` | `not-applicable` |
| `privileged` | `applications` | `applications/kvm-shell-environment` | `naked` | `native` | `not-applicable` |
| `privileged` | `backend-parity-c` | `backend-parity-c/cpuid-probe` | `naked` | `native` | `not-applicable` |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 103 / 104 | 0 / 104 | 0 / 104 | 103 | 312 |
| `bin-c` | 1 / 2 | 0 / 2 | 0 / 2 | 1 | 6 |
| `c-programs` | 159 / 165 | 0 / 165 | 2 / 165 | 161 | 495 |
| `chaos-c` | 1 / 1 | 0 / 1 | 1 / 1 | 2 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 5 / 6 | 0 / 6 | 1 / 6 | 6 | 18 |
| `determinism-stress-c` | 11 / 11 | 0 / 11 | 1 / 11 | 12 | 33 |
| `language-runtimes` | 18 / 19 | 0 / 19 | 0 / 19 | 18 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 33 / 34 | 1 / 34 | 0 / 34 | 34 | 102 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 730 selected Hermit-backend regression cells (including 5 chaos-mode race-exposure checks), 0 selected native controls, and 3 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.

### Selected custom commands outside the comparable denominator

These rows are part of the selected regression denominator even though they are not rows in `ci/compat-envelope/cells.json`. Their exact identities come from `ci/expected-e2e-plan.json`; `scorecard.rs check` refuses any selected row that is not accounted for by either this table or the comparable green cells above.

| Lane | Category | Test | Mode | Backend |
| --- | --- | --- | --- | --- |
| `portable` | `backend-parity-c` | `backend-parity-c/environment-and-workdir` | `custom` | `ptrace` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `liteinst` |
| `portable` | `system-utils` | `system-utils/clock-determinism` | `custom` | `ptrace` |

## Status and measurement

Selection and observation answer different questions. The Green/Red table says what full validation selects. The per-cell `measurement` value says what retained evidence observed: `never-measured`, `measured-and-passed`, `measured-no-verdict`, `diverged-unlocated`, or `diverged`. In the current generated data, **15 Green cells are `never-measured`**. Read the generated Status and measurement section for the complete current cross-tab; do not use Red as a failed-test count.

The current green/`never-measured` count is **15**, and the current red/`measured-and-passed` count is **93**.

Retained history that has not been imported is not counted here. A stored measurement does not establish that it describes current code; `show` reports whether the recorded last test still matches `HEAD:detcore`.

The cross-tab includes all **5400** tracked Hermit-backend cells; native controls remain visible in their separate table above. The current generated data contains **93 Red cells that are `measured-and-passed`**. These claims use the same counts printed in the table below.

| Status | `never-measured` | `measured-and-passed` | `measured-no-verdict` | `diverged-unlocated` | `diverged` | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `green` | 15 | 695 | 0 | 6 | 14 | 730 |
| `red` | 15 | 93 | 8 | 0 | 28 | 144 |
| `not-applicable` | 4525 | 0 | 0 | 0 | 1 | 4526 |
| **Total** | **4555** | **788** | **8** | **6** | **43** | **5400** |

Cells whose stored `measurement` is not `never-measured` are shown individually so status and measurement remain visible together.

| Test | Mode | Backend | Status | Measurement |
| --- | --- | --- | --- | --- |
| `applications/c-toolchain-workflow` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `kvm` | `red` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `ptrace` | `red` | `measured-and-passed` |
| `applications/git-repository-workflow` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `applications/timed-progress-bar` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `applications/timed-progress-bar` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cpu-virtualization` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/dup-shared-offset` | `verify` | `dbt` | `red` | `diverged` |
| `backend-parity-c/dup-shared-offset` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/dup-shared-offset` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/environment-and-workdir` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/epoll-pwait2` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fchmodat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fd-duplication` | `verify` | `dbt` | `red` | `diverged` |
| `backend-parity-c/fd-duplication` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/fd-duplication` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `kvm` | `green` | `diverged` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fsync-durability` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/getcpu-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/getpriority-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `dbt` | `red` | `diverged` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/host-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/host-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/inotify-watch` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/ioctl-fionread` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/lseek-positioning` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/msync-writeback` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/numa-node-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pid-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pidfd-open-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-multiwriter-ordering` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/poll-readiness` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/prctl-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/prctl-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/prctl-pdeathsig` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/preadv2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `kvm` | `red` | `diverged` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `sabre` | `red` | `diverged` |
| `backend-parity-c/readdir-entries` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/readdir-entries` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/rlimit-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/sched-getaffinity-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/signal-waitstatus-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/socket-epoll-ordering` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/socket-epoll-ordering` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/socket-options` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/socketpair-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sockname-unnamed` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/thp-disable` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/timer-family-identity` | `verify` | `ptrace` | `red` | `diverged` |
| `backend-parity-c/umask-mode` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/umask-mode` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/umask-mode` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/vectored-file-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `bin-c/posix-timer-test` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/acct-refusal-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/adjtimex-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/bpf-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/clock-adjtime-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/clone` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/copy-file-range-refusal-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-copied-tiocgpgrp` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-execveat-unsupported` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `c-programs/dbt-mmap-exec` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-pid-virtualization` | `verify` | `ptrace` | `red` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-prlimit-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-self-sigqueue` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-self-sigqueue` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-wait-lifecycle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-wait-lifecycle` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/epoll-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `chaos` | `ptrace` | `green` | `diverged-unlocated` |
| `c-programs/fp-reduction-nondeterminism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-requeue-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-wake-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-child` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/get-robust-list-child` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-child` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-thread` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-thread` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `c-programs/getcpu` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `chaos` | `ptrace` | `red` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/just-spin` | `verify` | `kvm` | `red` | `diverged` |
| `c-programs/just-spin` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/just-spin` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `c-programs/kcmp-eperm` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `sabre` | `green` | `diverged` |
| `c-programs/listmount-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/liteinst-advanced` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/lsm-get-self-attr-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/lsm-list-modules-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/lsm-set-self-attr-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/madvise-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/map-shadow-stack-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/memfd-secret-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/mmap-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-at-eopnotsupp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-directory-eopnotsupp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-empty-path-eopnotsupp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/nanosleep-par` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/nanosleep-threads-nocrash` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/nanosleep-threads-nocrash` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/netlink-autobind-generic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-generic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-route` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netlink-autobind-usersock` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/perf-event-hardware-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/perf-event-open-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/perf-event-software-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/perf-event-watchpoint-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `c-programs/pidfd-poll-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pidfd-waitid-child` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pidfd-waitid-child` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/prctl-dumpable` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/printf-with-threads` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/printf-with-threads` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/proc-fd-link-aliases` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/proc-fd-link-aliases` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/proc-fd-link-aliases` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/proc-fdinfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `ptrace` | `green` | `diverged` |
| `c-programs/proc-locks` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/process-mrelease-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/process-vm-readv-refusal-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/process-vm-writev-refusal-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/procfs-identity-agreement` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/procfs-identity-agreement` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `c-programs/procfs-positioned-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `kvm` | `red` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/prodcons-determinism` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/pselect6-simulation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ptrace-attach-eperm` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/ptrace-attach-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ptrace-attach-eperm` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ptrace-eperm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ptrace-seize-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ptrace-seize-eperm` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ptrace-traceme-eperm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/racewrite-nostdlib` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/racewrite-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/random-sources` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/record-replay-fd-close` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `c-programs/record-replay-file-state` | `verify` | `ptrace` | `red` | `diverged` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-anonymous-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-memfd-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `dbt` | `red` | `diverged` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/remap-file-pages-tmpfile-enosys` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/request-key-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `sabre` | `green` | `diverged` |
| `c-programs/sched-setattr-idle` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `sabre` | `green` | `diverged` |
| `c-programs/sched-setattr-other` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sched-yield-progress` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-yield-progress` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/scheduler-policy-queries` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigmask-preemption` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigmask-preemption` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/signal-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigpipe-siginfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigtimedwait-no-timeout` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-0s` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-0s` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `c-programs/sigtimedwait-timeout-1s` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp4` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-tcp6` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/so-incoming-cpu-udp4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-tcp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-udp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/socket-cookie-unix` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/socket-ioctl-timestamp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-ioctl-timestamp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timespec` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/socket-timestamp-timeval` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/splice-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/statmount-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `kvm` | `red` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sysfs-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sysinfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sysinfo-uptime` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syslog-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sysv-sem-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sysv-shm-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tee-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/thread-self-procfs-handoff` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/thread-self-procfs-handoff` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/thread-sync-determinism` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `verify` | `kvm` | `red` | `diverged` |
| `c-programs/thread-sync-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/threadexhaustion` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/threadexhaustion` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/timer-create-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-dgram` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-seqpacket` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ustat-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `dbt` | `red` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/vmsplice-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/wait-on-child` | `verify` | `kvm` | `green` | `diverged` |
| `c-programs/wait-on-child` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/writev-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `sabre` | `red` | `diverged` |
| `data-handling/archive-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/dd-partial-transfers` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/jq-json-transform` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/shell-pipeline` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `sabre` | `not-applicable` | `diverged` |
| `data-handling/zstd-multithread` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `determinism-stress/example-race` | `verify` | `kvm` | `green` | `diverged` |
| `determinism-stress/example-race` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/example-race` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `determinism-stress/order-violation` | `chaos` | `ptrace` | `red` | `measured-and-passed` |
| `determinism-stress/order-violation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/process-chains` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-contention` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-interleaving` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-output` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/fork-tree` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/fork-tree` | `verify` | `sabre` | `red` | `diverged` |
| `determinism-stress-c/lock-free` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/mmap-fork-shared` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/mmap-fork-shared` | `verify` | `sabre` | `red` | `diverged` |
| `determinism-stress-c/pid-tid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pid-tid-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pipe-chain` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pipe-chain` | `verify` | `sabre` | `red` | `diverged` |
| `determinism-stress-c/pipe-prefill` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/producer-consumer` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/signal-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-stress` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-stress` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `language-runtimes/bash-loop-pipe-time` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `dbt` | `red` | `diverged` |
| `language-runtimes/example-python-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/example-python-random` | `verify` | `sabre` | `red` | `diverged` |
| `language-runtimes/gawk-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/gawk-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/lua-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/m4-macro-mkstemp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/node-v8-jit` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `kvm` | `green` | `diverged` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/python-dict-hash-iteration` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `language-runtimes/python-hash-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/python-hash-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/python-hashseed` | `verify` | `ptrace` | `red` | `diverged` |
| `language-runtimes/python-io-subprocess-time` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `language-runtimes/python-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/python-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/ruby-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/tcl-rand-clock` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/auxv-loader-dump` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/clock-exec-continuity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/date-nanoseconds` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/example-devrand` | `verify` | `sabre` | `red` | `measured-no-verdict` |
| `system-utils/file-timestamp-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/file-timestamp-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/nscd-neutralised` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-enc` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/overflow-gid-resolves` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/proc-random-uuid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/procfs-sanitized-paths` | `verify` | `ptrace` | `red` | `diverged` |
| `system-utils/ps-proc-table` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `replay` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `liteinst` | `red` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `sabre` | `green` | `diverged` |
| `system-utils/shm-coherency-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/sort-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/ssh-keygen-ed25519` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `sabre` | `red` | `diverged` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `applications/kvm-python-examples` | `verify` | `kvm` | `red` | `diverged` |
| `applications/kvm-shell-environment` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |

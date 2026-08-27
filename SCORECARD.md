# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Selected** means the cell is listed in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Not selected** means an enabled cell is absent from that plan: measured failure, unavailable, or not yet run all remain not selected until the cell is promoted into the regression plan and passes. The summary table below classifies the current **5053** manifest-disabled combinations as **Not applicable**, not as not-selected or omitted: a cell that cannot run cannot pass or fail.

**Selected does not mean measured, and it does not mean passing.** Selection, measurement, and result are three separate facts, and the Selected column below reports only the first of them. It is a statement about what the plan REQUIRES, not about what has been OBSERVED. Whether a result was ever seen is a per-cell `measurement` field in `ci/compat-envelope/cells.json`, independent of selection and reading `never-measured`, `measured-and-passed`, or `diverged`; a cell can be selected and `never-measured`, or not selected and `measured-and-passed`. The generated Status and measurement section below states whether those combinations are present today and quotes their exact current counts. To count what has actually run, count that field -- do not count this table. Conflating the three has repeatedly produced project-status reports that quoted the Selected total as a number of passing tests, which it has never been.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Selected | Not selected | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 241 | 122 | 711 | 1074 |
| `dbt` | 0 | 61 | 1013 | 1074 |
| `kvm` | 1 | 21 | 1052 | 1074 |
| `sabre` | 57 | 86 | 931 | 1074 |
| `liteinst` | 5 | 48 | 1021 | 1074 |
| `native` | 0 | 33 | 325 | 358 |
| **Total** | **304** | **371** | **5053** | **5728** |

## Denominator, and why the percentage is not comparable across changes to it

Selected is **304 of 5728**, which is **5.31%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **5053 of those 5728 cells are NOT APPLICABLE** — their backend is not enabled for their mode, so they were never asked to run and cannot pass or fail. Over the 675 cells that CAN run, the selected share is **45.04%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 304 selected cells measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly not selected RAISES the reported figure; adding honest not-selected cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `selected / total`; an em dash means that mode does not exist for that backend. The summary columns use the same Selected, Not selected, and Not applicable statuses as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Selected | Not selected | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 235 / 358 | 0 / 358 | 1 / 358 | 57 / 358 | 5 / 358 | — | 298 | 336 | 1156 | 1790 |
| `replay` | 1 / 358 | 0 / 358 | 0 / 358 | 0 / 358 | 0 / 358 | — | 1 | 0 | 1789 | 1790 |
| `chaos` | 5 / 358 | 0 / 358 | 0 / 358 | 0 / 358 | 0 / 358 | — | 5 | 2 | 1783 | 1790 |
| `naked` | — | — | — | — | — | 0 / 358 | 0 | 33 | 325 | 358 |
| **Total** | | | | | | | **304** | **371** | **5053** | **5728** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `selected / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Selected | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 86 / 103 | 0 / 103 | 0 / 103 | 86 | 309 |
| `bin-c` | 0 / 2 | 0 / 2 | 0 / 2 | 0 | 6 |
| `c-programs` | 80 / 164 | 0 / 164 | 2 / 164 | 82 | 492 |
| `chaos-c` | 0 / 1 | 0 / 1 | 1 / 1 | 1 | 3 |
| `data-handling` | 6 / 6 | 0 / 6 | 0 / 6 | 6 | 18 |
| `debugger-c` | 1 / 1 | 0 / 1 | 0 / 1 | 1 | 3 |
| `determinism-stress` | 4 / 6 | 0 / 6 | 1 / 6 | 5 | 18 |
| `determinism-stress-c` | 7 / 11 | 0 / 11 | 1 / 11 | 8 | 33 |
| `language-runtimes` | 17 / 19 | 0 / 19 | 0 / 19 | 17 | 57 |
| `shared-futex-c` | 0 / 4 | 0 / 4 | 0 / 4 | 0 | 12 |
| `system-utils` | 31 / 34 | 1 / 34 | 0 / 34 | 32 | 102 |
| `util-c` | 0 / 1 | 0 / 1 | 0 / 1 | 0 | 3 |

Ordinary full validation executes 306 selected regression cells: the 304 selected compatibility cells above (including 5 chaos-mode race-exposure checks), and 2 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing selected cell is a regression, not permission to deselect it.

## Status and measurement

The table above reports status. This table reports the separate `measurement` field derived from observations stored in `ci/compat-envelope/cells.json`; it does not change status or which cells ordinary validation selects. Retained history that has not been imported is not counted here. A stored measurement does not establish that it describes current code; `show` reports whether the recorded last test still matches `HEAD:detcore`.

The count table includes all **5728** tracked cells; no row is omitted. The current selected/`never-measured` count is **0**, and the current not-selected/`measured-and-passed` count is **1**. These values use the same counts printed in the table below.

| Status | `never-measured` | `measured-and-passed` | `measured-no-verdict` | `diverged-unlocated` | `diverged` | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `selected` | 0 | 301 | 0 | 3 | 0 | 304 |
| `not-selected` | 362 | 1 | 0 | 0 | 8 | 371 |
| `not-applicable` | 5052 | 0 | 0 | 0 | 1 | 5053 |
| **Total** | **5414** | **302** | **0** | **3** | **9** | **5728** |

Cells whose stored `measurement` is not `never-measured` are shown individually so status and measurement remain visible together.

| Test | Mode | Backend | Status | Measurement |
| --- | --- | --- | --- | --- |
| `applications/c-toolchain-workflow` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `ptrace` | `not-selected` | `measured-and-passed` |
| `applications/git-repository-workflow` | `verify` | `ptrace` | `selected` | `diverged-unlocated` |
| `applications/timed-progress-bar` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/epoll-pwait2` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fchmodat2-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/fsync-durability` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `dbt` | `not-selected` | `diverged` |
| `backend-parity-c/host-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/inotify-watch` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/ioctl-fionread` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `liteinst` | `selected` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/msync-writeback` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pipe-multiwriter-ordering` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/poll-readiness` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/prctl-pdeathsig` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/preadv2-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `sabre` | `not-selected` | `diverged` |
| `backend-parity-c/readdir-entries` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/socket-options` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/socketpair-flags` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/sockname-unnamed` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `liteinst` | `selected` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/thp-disable` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/timer-family-identity` | `verify` | `ptrace` | `not-selected` | `diverged` |
| `backend-parity-c/umask-mode` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/vectored-file-io` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/clone` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `chaos` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/liteinst-advanced` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `liteinst` | `selected` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `sabre` | `not-selected` | `diverged` |
| `c-programs/prctl-dumpable` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/proc-fdinfo` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/pselect6-simulation` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/racewrite-nostdlib` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/random-sources` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/signal-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sigpipe-siginfo` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sigtimedwait-no-timeout` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-1s` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/sysinfo` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/sysinfo-uptime` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `chaos` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `verify` | `kvm` | `not-selected` | `diverged` |
| `c-programs/timer-create-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `liteinst` | `selected` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/wait-on-child` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `c-programs/writev-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `chaos` | `ptrace` | `selected` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `sabre` | `not-selected` | `diverged` |
| `data-handling/archive-roundtrip` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `data-handling/dd-partial-transfers` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `data-handling/jq-json-transform` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `data-handling/shell-pipeline` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `sabre` | `not-applicable` | `diverged` |
| `data-handling/zstd-multithread` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `liteinst` | `selected` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `determinism-stress/order-violation` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress/process-chains` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress/thread-contention` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress/thread-interleaving` | `chaos` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress/thread-output` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/lock-free` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/pid-tid` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/pid-tid-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/pipe-prefill` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/producer-consumer` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/signal-order` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `chaos` | `ptrace` | `selected` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/bash-loop-pipe-time` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/gawk-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/lua-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/m4-macro-mkstemp` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/node-v8-jit` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/python-dict-hash-iteration` | `verify` | `ptrace` | `selected` | `diverged-unlocated` |
| `language-runtimes/python-hash-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/python-hashseed` | `verify` | `ptrace` | `not-selected` | `diverged` |
| `language-runtimes/python-io-subprocess-time` | `verify` | `ptrace` | `selected` | `diverged-unlocated` |
| `language-runtimes/python-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/ruby-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `language-runtimes/tcl-rand-clock` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/auxv-loader-dump` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/clock-exec-continuity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/date-nanoseconds` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/file-timestamp-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/nscd-neutralised` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/openssl-enc` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/overflow-gid-resolves` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/proc-random-uuid` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/ps-proc-table` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/record-getpid` | `replay` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `sabre` | `selected` | `measured-and-passed` |
| `system-utils/shm-coherency-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/sort-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/ssh-keygen-ed25519` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `sabre` | `not-selected` | `diverged` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `ptrace` | `selected` | `measured-and-passed` |
| `applications/kvm-python-examples` | `verify` | `kvm` | `selected` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `ptrace` | `selected` | `measured-and-passed` |

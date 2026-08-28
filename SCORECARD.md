# Compatibility scorecard

This table is derived from the manifest, not from a separately maintained parent-workspace CSV. `./ci/compat-envelope/scorecard.rs check` verifies it.

**Green** means the cell is SELECTED: it is listed in `ci/expected-e2e-plan.json` and is therefore required to pass by ordinary validation. **Red** means an enabled cell is not selected: measured failure, unavailable, or not yet run all remain red until the cell is promoted into the regression plan and passes. The summary table below classifies the current **5068** manifest-disabled combinations as **Not applicable**, not red or omitted: a cell that cannot run cannot pass or fail.

**Green does not mean measured, and it does not mean passing.** Selection, measurement, and result are three separate facts, and the Green column below reports only the first of them. Green is a statement about what the plan REQUIRES, not about what has been OBSERVED. Whether a result was ever seen is a per-cell `measurement` field in `ci/compat-envelope/cells.json`, independent of colour and reading `never-measured`, `measured-and-passed`, or `diverged`; a cell can be green and `never-measured`, or red and `measured-and-passed`. The generated Status and measurement section below states whether those combinations are present today and quotes their exact current counts. To count what has actually run, count that field -- do not count this table. Conflating the three has repeatedly produced project-status reports that quoted the Green total as a number of passing tests, which it has never been.

Every selected `verify` cell, and every seed in a selected `chaos` cell, runs the same backend twice. The manifest runner adds `--verify-strict` when the selected Hermit binary supports it, and accepts a result only when the typed report says `verified=true`, `verdict=matched`, `bitwise_parity=true`, `strictness=canonical`, `compare_logs=true`, a named canonical `record_envelope`, and both INFO-message counts are nonzero. Bare `--verify` remains a Stripped comparison when invoked directly and does not satisfy this regression plan. These same-backend results do not establish cross-backend parity.

| Backend | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: |
| `ptrace` | 242 | 122 | 713 | 1077 |
| `dbt` | 0 | 61 | 1016 | 1077 |
| `kvm` | 1 | 21 | 1055 | 1077 |
| `sabre` | 56 | 87 | 934 | 1077 |
| `liteinst` | 5 | 48 | 1024 | 1077 |
| `native` | 0 | 33 | 326 | 359 |
| **Total** | **304** | **372** | **5068** | **5744** |

## Denominator, and why the percentage is not comparable across changes to it

Green is **304 of 5744**, which is **5.29%** — over THIS population and no other. The population is every combination the manifest declares, and it is composed of:

- backends: `ptrace`, `dbt`, `kvm`, `sabre`, `liteinst`, `native`
- modes: `chaos`, `naked`, `replay`, `verify`

⚠️ **5068 of those 5744 cells are NOT APPLICABLE** — their backend is not enabled for their mode, so they were never asked to run and cannot pass or fail. Over the 676 cells that CAN run, green is **44.97%**.

⚠️ **DO NOT QUOTE THAT SECOND FIGURE AS PROGRESS.** It is the same 304 green cells measured against a smaller denominator. Nothing was fixed to produce it; it is what the first figure always meant once the cells that cannot run are excluded. Quote both or neither, and never compare one against the other as though something moved.

⚠️ **Adding or removing a backend or mode changes this denominator and therefore the percentage, without anything about the product changing.** Removing a backend whose cells are mostly red RAISES the reported figure; adding honest red cells LOWERS it. Neither is progress. Before comparing this percentage against an earlier one, diff the two lists above: if they differ, the numbers are not comparable and the difference is not a result.

The mode view makes the current order of work explicit: expand `verify` first, then `replay`, then `chaos`. Each backend cell is `green / total`; an em dash means that mode does not exist for that backend. The summary columns use the same Green, Red, and Not applicable statuses as the table above.

| Mode | `ptrace` | `dbt` | `kvm` | `sabre` | `liteinst` | `native` | Green | Red | Not applicable | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `verify` | 236 / 359 | 0 / 359 | 1 / 359 | 56 / 359 | 5 / 359 | — | 298 | 337 | 1160 | 1795 |
| `replay` | 1 / 359 | 0 / 359 | 0 / 359 | 0 / 359 | 0 / 359 | — | 1 | 0 | 1794 | 1795 |
| `chaos` | 5 / 359 | 0 / 359 | 0 / 359 | 0 / 359 | 0 / 359 | — | 5 | 2 | 1788 | 1795 |
| `naked` | — | — | — | — | — | 0 / 359 | 0 | 33 | 326 | 359 |
| **Total** | | | | | | | **304** | **372** | **5068** | **5744** |

## Cross-backend parity

The manifest-backed scorecard does not yet contain cross-backend parity cells. In particular, a DBT, KVM, SaBRe, or LiteInst `verify` cell compares that backend with itself, not with ptrace. Standalone backend gates exercise selected comparisons, but their results are not counted here. Until a cell actually compares a fresh ptrace log with the corresponding backend log, this table reports no cross-backend parity number.

## Ptrace by manifest category

This view uses the same Basic Sanity Milestone 1 contracts as the tables above, but makes the ptrace workload mix visible. Each entry is `green / total`; `custom` commands are not part of this denominator.

| Manifest category | Verify | Replay | Chaos | Green | Total |
| --- | ---: | ---: | ---: | ---: | ---: |
| `applications` | 3 / 6 | 0 / 6 | 0 / 6 | 3 | 18 |
| `backend-parity-c` | 87 / 104 | 0 / 104 | 0 / 104 | 87 | 312 |
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

Ordinary full validation executes 307 selected regression cells: the 304 green compatibility cells above (including 5 chaos-mode race-exposure checks), and 3 explicit custom commands outside the comparable denominator. A passing validate must produce a fresh result for all of them; a failing green cell is a regression, not permission to move it to red.

## Status and measurement

The table above reports status. This table reports the separate `measurement` field derived from observations stored in `ci/compat-envelope/cells.json`; it does not change status or which cells ordinary validation selects. Retained history that has not been imported is not counted here. A stored measurement does not establish that it describes current code; `show` reports whether the recorded last test still matches `HEAD:detcore`.

The count table includes all **5744** tracked cells; no row is omitted. The current green/`never-measured` count is **0**, and the current red/`measured-and-passed` count is **2**. These values use the same counts printed in the table below.

| Status | `never-measured` | `measured-and-passed` | `measured-no-verdict` | `diverged-unlocated` | `diverged` | Total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `green` | 0 | 301 | 0 | 3 | 0 | 304 |
| `red` | 362 | 2 | 0 | 0 | 8 | 372 |
| `not-applicable` | 5067 | 0 | 0 | 0 | 1 | 5068 |
| **Total** | **5429** | **303** | **0** | **3** | **9** | **5744** |

Cells whose stored `measurement` is not `never-measured` are shown individually so status and measurement remain visible together.

| Test | Mode | Backend | Status | Measurement |
| --- | --- | --- | --- | --- |
| `applications/c-toolchain-workflow` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `applications/example-timed-progress-bar` | `verify` | `ptrace` | `red` | `measured-and-passed` |
| `applications/git-repository-workflow` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `applications/timed-progress-bar` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/aio-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/append-pwrite` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/bind-getsockname` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/cachestat-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/child-subreaper-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/close-range-fds` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/copy-file-range-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/cwd-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/environment-and-workdir` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/epoll-pwait2` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/epoll-readiness` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/event-delivery-ordering` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/eventfd-semantics` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/faccessat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fadvise-hints` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fallocate-extents` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fchmod-bits` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fchmodat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fcntl-owner` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/file-backed-mmap` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/file-io-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/flock-lifecycle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fork-exec-pipeline` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/fsync-durability` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/ftruncate-sparse` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/getrusage-self-accounting` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/hardware-trap-identity` | `verify` | `dbt` | `red` | `diverged` |
| `backend-parity-c/host-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/inline-syscall-sites` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/inotify-watch` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/ioctl-fionread` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/kcmp-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/linkat-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mce-kill-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/membarrier-query` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/memfd-create` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mempolicy-default` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mincore-residency` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mixed-inline-and-libc-syscalls` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mkdir-rmdir` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mknod-special` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `backend-parity-c/mmap-layout-pointer-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/msync-writeback` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/name-to-handle-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/no-new-privs-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/o-tmpfile-anon` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/openat-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/openat2-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/path-file-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/personality-domain` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-capacity-pin` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-ipc` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe-multiwriter-ordering` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pipe2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/poll-readiness` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/prctl-pdeathsig` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/preadv2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/pthread-lifecycle` | `verify` | `sabre` | `red` | `diverged` |
| `backend-parity-c/readdir-entries` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/readdir-order-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/record-lock` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/rename-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/renameat2-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/robust-list` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/seccomp-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sendfile-copy` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/set-tid-address` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/short-io-split-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/shutdown-socketpair` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/signal-delivery-sequence` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/signalfd-create` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/socket-options` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/socketpair-flags` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sockname-unnamed` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `backend-parity-c/stat-metadata-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/statfs-free-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/static-nolibc-syscall-sites` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/statx-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/symlink-ops` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sync-file-range` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/sysv-ipc-refusal` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/thp-disable` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/timer-family-identity` | `verify` | `ptrace` | `red` | `diverged` |
| `backend-parity-c/umask-mode` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/uname-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/utimensat-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/vectored-file-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `backend-parity-c/vectored-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/add-key-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/arch-prctl-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/cachestat-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/clone` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-exec-failure` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/dbt-mmap-exec` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/fp-reduction-nondeterminism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/futex-waitv-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/get-robust-list-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getcpu` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getitimer-determinism-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/getsockopt-null` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-alarm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/hello-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/hello-signals` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/io-uring-fallback` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/io-uring-ring-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ioctl-fioclex` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ioctl-siocethtool` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ipc-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/kcmp-eperm` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/keyctl-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/keyctl-passthrough` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/listmount-enosys` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/liteinst-advanced` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-available-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-cached-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/meminfo-free-deterministic` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/memorypress` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/mmap-stress-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/name-to-handle-regular-eopnotsupp` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/nanosleep-par` | `verify` | `sabre` | `red` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-tcp6` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/netns-cookie-udp4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pause-alarm-interrupt` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/periodic-setitimer-delivery` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pidfd-open-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pidfd-poll-self` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pipe2-errno-precedence` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/ppoll-readv` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/ppoll-simulation` | `verify` | `sabre` | `red` | `diverged` |
| `c-programs/prctl-dumpable` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/prctl-dumpable` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/prctl-option-policy` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pread64-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/print-memaddrs` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/proc-fdinfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/proc-locks` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/procfs-positioned-probe` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/pselect6-simulation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/pty-nr-count` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/racewrite-nostdlib` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/random-sources` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/rcx-canonicalization` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/record-replay-lseek-seek-cur` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/record-replay-setsockopt` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/recvmsg-scm-rights-mmap` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-batch` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-idle` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sched-setattr-other` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/scheduler-policy-queries` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/session-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/setitimer-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/signal-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigpipe-siginfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigtimedwait-no-timeout` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sigtimedwait-timeout-1s` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-io` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-file-metadata` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/syscall-quick-wins` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/sysinfo` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/sysinfo-uptime` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-accept6` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/tcp-info-client4` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/thread-sync-determinism` | `verify` | `kvm` | `red` | `diverged` |
| `c-programs/timer-create-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/timer-create-determinism` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/uname` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/unix-autobind-stream` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `c-programs/vforkexec` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/wait-on-child` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `c-programs/writev-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `chaos-c/lock-granularity` | `verify` | `sabre` | `red` | `diverged` |
| `data-handling/archive-roundtrip` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/dd-partial-transfers` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/jq-json-transform` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/shell-pipeline` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `data-handling/sqlite-query-determinism` | `verify` | `sabre` | `not-applicable` | `diverged` |
| `data-handling/zstd-multithread` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `liteinst` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `debugger-c/debuggee` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `determinism-stress/order-violation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/process-chains` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-contention` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-interleaving` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress/thread-output` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/lock-free` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pid-tid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pid-tid-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/pipe-prefill` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/producer-consumer` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/signal-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `chaos` | `ptrace` | `green` | `measured-and-passed` |
| `determinism-stress-c/thread-contention` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/bash-loop-pipe-time` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/bash-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/cpp-stl-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/gawk-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/lua-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/m4-macro-mkstemp` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/node-v8-jit` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-hash-order` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-io-subprocess-time` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/perl-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/python-dict-hash-iteration` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `language-runtimes/python-hash-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/python-hashseed` | `verify` | `ptrace` | `red` | `diverged` |
| `language-runtimes/python-io-subprocess-time` | `verify` | `ptrace` | `green` | `diverged-unlocated` |
| `language-runtimes/python-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/ruby-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/rust-hashmap-iteration` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `language-runtimes/tcl-rand-clock` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/auxv-loader-dump` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/clock-determinism` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/clock-exec-continuity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/date-nanoseconds` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/du-tree-summary` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/errno-path-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/example-date` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/file-timestamp-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/find-tree-metadata` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/harness-width-contract` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/mcookie-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/mktemp-name` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/nscd-neutralised` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-enc` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-genpkey` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-passwd` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-rand` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/openssl-x509` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/overflow-gid-resolves` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/proc-random-uuid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/proc-uptime` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/ps-proc-table` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/random-device` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `replay` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/record-getpid` | `verify` | `sabre` | `green` | `measured-and-passed` |
| `system-utils/shm-coherency-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/shuf-permutation` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/sort-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/ssh-keygen-ed25519` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/startup-surface-identity` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/startup-tls-guards` | `verify` | `sabre` | `red` | `diverged` |
| `system-utils/sysfs-sanitized-prefixes` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `system-utils/uuidgen-random` | `verify` | `ptrace` | `green` | `measured-and-passed` |
| `applications/kvm-python-examples` | `verify` | `kvm` | `green` | `measured-and-passed` |
| `backend-parity-c/cpuid-probe` | `verify` | `ptrace` | `green` | `measured-and-passed` |

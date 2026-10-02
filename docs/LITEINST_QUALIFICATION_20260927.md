# LiteInst qualification: 162 verify cells, 2026-09-27

This change enables and selects 162 previously disabled LiteInst `verify`
cells. Each cell has ten clean first-attempt strict repetitions from the
official pressure runner, all on the ordinary portable lane, and an independent
audit read every raw result row. Every selected cell's ptrace `verify` cell is
already selected by full validation, so this ratchets the host hybrid toward
the ptrace-green set. No cell was gained by an exemption, filter, comparator
change, bound change or manifest weakening.

The independent audit qualified 166 cells, and four of them are not
selected:

- `system-utils/sort-random` diverged in the ten-repetition current-source
  screen at the author base. The failure is quoted in the screen section.
- `backend-parity-c/environment-and-workdir` and
  `backend-parity-c/pipe-multiwriter-ordering` would run in the
  `backend-parity-c` node. When this change was written, that node compared
  every LiteInst cell against a ptrace reference run, and these two were never
  measured under that comparison. Hermit `main` has since removed that
  comparison (`b9ec113b5f`, `b280bc4807`) and folded the bucket into
  `c-programs` (`34462d7afbc`), so the two cells are now
  `c-programs/environment-and-workdir` and
  `c-programs/pipe-multiwriter-ordering`. They stay disabled because enabling
  them under same-backend verification is a separate decision.
- `language-runtimes/perl-io-subprocess-time` passed 10/10 in the census
  (`qual10-batch1`), but the exact-head full validation of this change failed
  it on both attempts with a guest-visible `ERESTARTSYS` (-512) leak on
  `close()`. The receipt is quoted in its own section below.

Each of the four keeps its LiteInst disabled entry, restored byte-for-byte
from the base.

## Identities

- **Measured source.** Hermit `19553a64308ca123bbde7cc3d7720e0aa72e795a`,
  Detcore tree `5577d908861170c5b5e89a47ce4460a80008afef`, clean tree
  (`source_tree_dirty=false` in every batch's `run.json`).
- **Pinned submodules.** Reverie `b0ede531e00dd1e068d0e0b1f220edddcf96d4b5`,
  agent-utils `26dd3eaa34ea9b22a825201f1ddc37018fb0ed22` and rr
  `39e5c18e7e43236b7ca0fb1eb647fe9c93e3934e`. The census verified that the
  staged `libreverie_liteinst.so` carries the Reverie `b0ede531` revision marker
  and that the e2e artifact copy is byte-identical. A negative control with a
  zeroed marker was refused with exit 125.
- **Author base.** `b63af4583ad01db84a641bc3ade267f4f60e20da`, whose Detcore
  tree is `38e13c0fcc41d93f07f3df90371d01591105072d`. It is exactly one commit
  after the measured source: "Make sysinfo uptime independent of epoch
  fraction". That commit changes:
  - `detcore/src/syscalls/sysinfo.rs`;
  - the `tests/c/sysinfo_uptime.c` guest;
  - the `tests/backend-parity/fixtures/host_identity.c` fixture;
  - `hermit-cli/src/metadata.rs`, whose `RECORD_VERSION` moves from `0x11a`
    to `0x11b` (record/replay only);
  - DAG test counts.

  This is shared runtime code on the host-hybrid path, so it is not a
  metadata-only difference. `git diff --quiet 19553a64 b63af4583a --
  tests/e2e/manifests ci/manifest-plan/src/timeouts.rs` exits 0.
- **Cells that reach the change.** The audit counted inbound `sysinfo(`
  records in all 20 retained INFO logs of each selected cell. Four of the 166
  audited cells reach it:
  - `c-programs/sysinfo`, 2 calls per run;
  - `c-programs/sysinfo-uptime`, 3 calls per run. Its guest source changed in
    the author base, so the measured guest binary is stale;
  - `system-utils/auxv-loader-dump`, 2 calls per run;
  - `system-utils/sort-random`, 2 calls per run. It diverged in the
    current-source screen and is not selected.

  Every other audited cell makes zero `sysinfo` calls in all 20 logs.
  `/proc/uptime` (`calculate_uptime`) is unchanged. The current-source screen
  below reruns all 166 cells at the author base, and reruns these four ten
  times each.
- **Base of this change.** The branch is based on Hermit `main`
  `03bbb83581fad247251df6363f50e61e24c2957e` ("Run the corpus-only lane as one
  bucket node instead of 189 probes"), 256 commits after the author base. The
  next paragraphs record the earlier bases first, up to round 2's
  `b280bc4807`, and then the 165 commits from `b280bc4807` to `03bbb83581f`.

  Round 2 was based on Hermit `main`
  `b280bc4807bf1a4baff74a09e45aaf6778c09fe7` ("Remove the ptrace parity rerun
  from test-harness"), 91 commits after the author base. It was first based on
  `55214f50e4dd`, which is `bffdf33788ca` plus one unrelated commit that is not
  on `main` and only touches the GDB helper test
  (`hermit-cli/src/bin/hermit/gdb_client.rs`). It was then rebased onto
  `bffdf33788ca`, dropping that commit, then onto `e63236584625`, and then
  onto `b280bc4807`. The last rebase's only textual conflict was in the
  generated `ci/compat-envelope/cells.json`, which was regenerated with the
  new parity snapshot and pinned parity counts. The Reverie pin (`6297f715`) and agent-utils
  pin (`26dd3eaa34`) are the same at `e63236584625` and `b280bc4807`.

  The delta from the author base to `e63236584625` is three commits:
  - the KVM process-retirement fence in `detcore/src/scheduler/` and
    `detcore/src/tool_global.rs`, with its tests and DAG test counts;
  - a Reverie pin move from `b0ede531` to `a1d07619`. Its changes are in
    `reverie-kvm` plus one new `GlobalTool::on_backend_process_retired`
    callback in `reverie/src/tool.rs`, which does nothing by default;
  - "Advance Reverie for permission-aware KVM wait4" (`e63236584625`), a
    Reverie pin move from `a1d07619` to `6297f715`
    (https://github.com/rrnewton/reverie/pull/668). In Reverie it changes only
    five files under `reverie-kvm/` (`src/executor.rs`,
    `src/process_signal_publication.rs`, `src/wait4_copyout_tests.rs`,
    `tests/fixtures/leader_wait_status.c`, `tests/support/leader_exit.rs`);
    `git diff --stat a1d07619 6297f715 -- ':!reverie-kvm'` is empty. In Hermit
    it changes only the Reverie pin: every Cargo manifest's `rev`, both
    lockfiles, the `reverie` gitlink, `HERMIT_REVERIE_PIN` in
    `hermit-cli/BUCK`, and the DBT build-budget pin in
    `ci/configure-build-jobs.sh` and `ci/run-with-reverie-dbt-budget.sh`.

  The new Detcore retirement fence is only armed when a backend installs
  `BackendSignalControl`. In the pinned Reverie source only `reverie-kvm`
  provides it. The `reverie-kvm` wait4 change runs only under the KVM
  backend, and the LiteInst runtime crates (`reverie-liteinst`,
  `reverie-preload`, `reverie-syscalls`, `reverie-process`, `reverie-memory`)
  are byte-identical from the measured pin `b0ede531` to `6297f715`
  (`git diff --quiet` exits 0).

  The delta from `e63236584625` to `b280bc4807` is 88 commits. None of them is
  an ancestor of `19553a64`, `b63af4583a` or `e63236584625`. Thirty touch
  `detcore/`, `detcore-model/`, `hermit-cli/`, `tests/c/` or
  `tests/e2e/manifests/`. The ones that can change what a selected cell's
  guest sees or how it runs are:
  - **A rewritten guest for a selected cell.** "Require exact fixed-epoch
    clock parity between Cargo and Buck" (`b9ecf41ad2`) and "Anchor the Buck
    clock-parity epoch on CLOCK_REALTIME only" (`465056029c`) change 182
    lines (173 added, 9 removed) in `tests/c/clock_exec_continuity.c`, the
    guest of the selected cell
    `system-utils/clock-exec-continuity`. Each of its three exec generations
    now runs four bracketed work segments (0, 100,000, 100,000 and 200,000
    Collatz iterations) and two serialized pthreads that each read the clock
    four times, and it reads `CLOCK_REALTIME`. The manifest adds `-pthread`
    to its `cflags`. The census and the author-base screen measured the old
    single-threaded program.
  - **sysinfo, uptime and `btime`.** "Fix runtime semantics exposed by
    portable validation" (`72c333f0a0`), "Round sysinfo uptime up as Linux
    does" (`6b7d0d0b5a`), "Fix btime, Ubuntu unwind closure, and runtime-only
    admission" (`f41ed8e34b`) and "Compute btime exactly and only for
    /proc/stat" (`0c5ce50f11`) change `detcore/src/syscalls/sysinfo.rs`,
    `detcore/src/procfs.rs`, `detcore/src/syscalls/files.rs`,
    `detcore/src/fd.rs`, `detcore-model/src/config.rs` and
    `detcore-model/src/time.rs`. "Advance the record version for the procfs
    uptime projection" (`9b1ad63d79`) changes only the record format version.
  - **Virtual time.** "Refuse final virtual time behind its baseline in
    release builds too" (`42aca806f9`) and "Complete Buck release runtime
    artifact" (`9450988081`) change `detcore-model/src/time.rs` and
    `detcore/src/tool_global.rs`.
  - **POSIX timers across exec.** "Delete process POSIX timers on successful
    exec" (`6ffdf53c01`) changes `detcore/src/scheduler.rs`,
    `detcore/src/scheduler/timed_waiters.rs`, `detcore/src/tool_global.rs`,
    `detcore/src/tool_local.rs` and `detcore/src/lib.rs`.
  - **`/proc` mounting.** "Allow read-only proc fallback in Hermit
    containers" (`e46e369da8`) and "Report read-only proc mounts and
    recording mode mismatches" (`0520fe4408`) change
    `hermit-cli/src/bin/hermit/container.rs`, the new
    `hermit-cli/src/proc_mount.rs`, and `run.rs`, `record.rs`, `replay.rs`
    and `metadata.rs`.
  - **Backend execution.** "Heap-pin backend execution before deadline
    handling" (`822ff607eb`) changes `hermit-cli/src/lib.rs`.

  The rest are the log-diff report schema (`d550979ad0`), the Buck release
  build (`ea7a341ffe`, `932ca9ee26` and the `BUCK` part of `9450988081`), the
  GDB helper (`e4c9786ce1`, `d90336ee6f`), the parity overhaul (`71b5bca69b`,
  `b9ec113b5f`, `b280bc4807`), tests, and CI. `git diff --quiet e63236584625
  b280bc4807 -- ci/manifest-plan/src/timeouts.rs` exits 0, and in
  `tests/e2e/manifests/` the only change to a selected recipe is the
  `-pthread` flag above. The LiteInst screen of this delta is in "Current-source
  screen at `b280bc4807`" below.

  Round 3 re-derives the branch onto `03bbb83581f`. All 165 commits from
  `b280bc4807` to `03bbb83581f` are first-parent commits. None of them is an ancestor of `19553a64`, which is
  itself an ancestor of `b280bc4807`. At `03bbb83581f` the `reverie` gitlink,
  `HERMIT_REVERIE_PIN` in `hermit-cli/BUCK` and the `rev` in
  `liteinst-runtime-build/runtime/Cargo.toml` all name Reverie `dbf2b5c8`. The
  pin moved five times in this delta:
  - "Advance Reverie to 2eeb704c for the launcher execve and SaBRe relocation
    fixes" (`511f2c74c74`);
  - "Advance Reverie to fba351a5 to match the fbsource import"
    (`e454d6c2819`);
  - "Advance Reverie to e9f88def for the LiteInst runtime-bootstrap window"
    (`1509006d2d9`);
  - "Advance Reverie to 5ef75860 for the DBT evidence accessor"
    (`0e23480d5e8`);
  - "Pin Reverie at dbf2b5c8 for the SaBRe loader metadata arena"
    (`098a9a40c09`).

  The agent-utils pin moved from `26dd3eaa34` to `d510a54892` (`76980bac899`
  and `fb3c2aaa311`). The manifests were also reorganised:
  - "Fold the backend-parity-c manifest bucket into c-programs"
    (`34462d7afbc`) moves that bucket into `c-programs.yaml`;
  - "Run the portable strict compatibility corpus as one manifest bucket"
    (`18ece74f8b0`) adds `compat.yaml`.

  None of the 162 selected cells was in `backend-parity-c`, and each of them is
  in the same manifest file at both commits. Two cells were qualified but left
  unselected because of the old `backend-parity-c` node. They are now
  `c-programs/environment-and-workdir` and
  `c-programs/pipe-multiwriter-ordering`, and
  `tests/e2e/manifests/inventory/retired-ids.json` maps their old ids to the
  new ones. Their guests moved from `tests/backend-parity/fixtures/` to
  `tests/c/`, and they stay outside the 162.

  The delta from `b280bc4807` to `03bbb83581f` is 165 commits:
  - 68 touch `detcore/`, `detcore-model/`, `hermit-cli/`, `tests/c/` or
    `tests/e2e/manifests/`;
  - 24 of those 68 touch `detcore/` or `detcore-model/`;
  - 5 touch `liteinst-runtime-build/`.

  The ones that can change what a selected cell's guest sees or how it runs
  are:
  - **The same guest input paths for every verify cell.** Three commits make
    this change:
    - "Give ptrace and candidate verify cells the same guest input paths"
      (`00d0aef544b`, https://github.com/rrnewton/hermit/issues/3301);
    - "Report parity credit as clean only when both launches were equalized"
      (`abaa7a78489`);
    - "Test that ptrace given a candidate's inputs matches ptrace given its
      own" (`6419f0fd02b`).

    They change `ci/manifest-plan/src/runner.rs`, which gained 3,364 lines
    and lost 179 over 24 commits in this delta. The runner now treats every
    `verify` cell whose backend is not DBT this way. The test is
    `equalizes_guest_inputs`, which returns
    `mode == "verify" && backend != "dbt"`. For each such cell, the runner:
    - binds the cell's `home`, `xdg-config` and `fixtures` directories at
      `/tmp/e2e/home`, `/tmp/e2e/xdg-config` and `/tmp/e2e/fixtures`;
    - points `HOME`, `XDG_CONFIG_HOME` and `E2E_FIXTURE_DIR` at those guest
      paths;
    - launches the guest as `/tmp/e2e/fixtures/program`;
    - rewrites path arguments to match.

    Every one of the 162 LiteInst cells, and every ptrace `verify` cell, now
    starts from a different executable path and environment than at
    `b280bc4807`, and any path in its arguments is rewritten. `language-runtimes/cpp-stl-determinism` also reads
    `E2E_FIXTURE_DIR`, so the fixture path it sees has changed as well.
  - **The LiteInst bootstrap window.** Five Hermit commits make this change:
    - "Stop charging the LiteInst runtime bootstrap to guest virtual time"
      (`a95ee68b0e6`, which names Reverie
      https://github.com/rrnewton/reverie/pull/771);
    - "Keep the clock moving inside the LiteInst bootstrap window"
      (`a4785c5e985`);
    - "Raise the LiteInst bootstrap cap, charge adjtimex, report a full
      window" (`b16ff9a2092`);
    - two test commits, `b7848321f09` and `b37fd1f4f06`.

    `1509006d2d9` brings in the Reverie half. Together they change Detcore's
    handling of LiteInst runs only. The bootstrap window is the stretch of time
    on one thread from the LiteInst runtime's validated start trap to its
    "ready" or "failed" report. During that stretch, the preloaded runtime
    prepares its patching before the guest's `main`. Detcore no longer charges
    the syscalls in this window to the guest's virtual time. The exception is
    the syscalls that read time, which are still charged: `gettimeofday`,
    `time`, `clock_gettime`, `sysinfo`, `times`, `getrusage`,
    `timerfd_gettime`, `timer_gettime`, `getitimer`, `adjtimex` and
    `clock_adjtime`. A window may contain at most 32,768 uncharged syscalls.
    This limit is `MAX_UNCHARGED_BOOTSTRAP_SYSCALLS`, raised from 4,096. With
    Reverie `dbf2b5c8`, the window for `host_identity.c` contains 315 syscalls.
    The change moves LiteInst's `sysinfo` uptime from 123 to 121, which is
    ptrace's value (https://github.com/rrnewton/hermit/issues/3338). As a
    result:
    - every clock a LiteInst guest reads after startup reads earlier than
      before;
    - the virtual times of committed scheduler turns change;
    - ptrace runs are not affected.
  - **Thread identity across `exec`.** "Reconnect Detcore identity after
    ptrace nonleader exec and order exec-time teardown" (`1aae8a2d12c`,
    https://github.com/rrnewton/hermit/pull/3268) applies when a thread that is
    not the thread-group leader calls `exec`. After the call, Detcore
    reconnects the surviving thread's identity. It then retires the sibling
    threads that `exec` killed, and their INFO log records, in thread-id order.
  - **The empty-queue log line.** "Log the empty-queue fizzle once per logical
    empty state" (`8e1fd974d30`, https://github.com/rrnewton/hermit/issues/3360,
    https://github.com/rrnewton/hermit/issues/3223) logs the INFO line "zero
    threads left anywhere, fizzling." once for each logical empty state.
    `--verify` compares INFO records, so the compared log changes for any cell
    that used to log this line more than once in one empty state.
  - **One stated epoch for both verify runs.** "Replay from the recording's
    virtual-time epoch" (`b8d11bab8a9`,
    https://github.com/rrnewton/hermit/issues/3411) makes replay use the
    recording's epoch, the starting value of virtual time. It also makes
    `hermit-verify` pass one explicit `--epoch=` to every run it launches.
  - **The injected `fstat` buffer.** "Record new-descriptor metadata even when
    the stack below rsp is unwritable" (`3b5f7cdc432`) adds a fallback buffer
    for the `fstat` call that Detcore injects for a new file descriptor. The
    fallback is used only when writing to the stack's scratch space faults; the
    usual path is unchanged.
  - **The recipes of eight selected cells.** "Carry the DBT strict parity
    matrix's 28 cases as manifest verify cells" (`2be6440ddd6`) changes the
    manifest entries of eight selected cells. In all eight, the LiteInst entry
    is still disabled. Each cell gains an `expected_stdout` table with
    `ptrace` and `dbt` entries only. The runner refuses a key that names a
    backend outside `backends_enabled`, so a `liteinst` entry can be added only
    once LiteInst is enabled. This change adds one to all eight, equal to the
    `ptrace` entry byte for byte; see "The eight `expected_stdout` entries"
    below.
    The other recipe changes are:
    - `c-programs/io-uring-fallback` ("io_uring blocked; epoll fallback
      ready"): `dbt` is added to `backends_enabled`. Verify `ci` changes from
      the scalar `true` to the per-backend map `{ptrace: true, sabre: true,
      kvm: true, dbt: false}`. `ci_disabled_reason.dbt` cites a parity failure
      (https://github.com/rrnewton/reverie/issues/764), and the commit adds
      custom ptrace and dbt cells. The re-derivation therefore adds
      `liteinst: true` to this map instead of relying on the old scalar.
    - `c-programs/listmount-enosys`, `c-programs/process-vm-readv-refusal-probe`
      and `c-programs/process-vm-writev-refusal-probe`: `ci.dbt` changes from
      `false` to `true`, and the DBT `ci_disabled_reason` is removed.
    - `c-programs/madvise-determinism` and `c-programs/syscall-file-metadata`:
      `dbt` is added to `backends_enabled`, and `ci.dbt` is set to `true`.
      `syscall-file-metadata` keeps `kvm: false`.
    - `c-programs/scheduler-policy-queries` and `c-programs/syscall-file-io`:
      `dbt` is added to `backends_enabled`.
  - **Guest sources.** The 162 cells run 157 distinct programs (127 C, 29
    shell, 1 Rust). The other 5 are `direct` cells, which run a command line
    instead of a program built from the tree. Of the 157 programs, 96 changed
    between the two commits. These counts come from comparing the two commits,
    not the working tree. The runner compiles every C guest with
    `-std=c11 -O2 -g -Wall -Wextra -Werror` at both commits. Because of `-g`,
    even a whitespace edit changes the binary's debug line table. Whether the
    loaded code differs was not checked. Of the 95 changed C guests, only
    `c-programs/sigtimedwait-timeout-0s` uses `assert` or `__LINE__`, which
    embed line numbers in the code. The 96 changed programs fall into four
    groups:
    - 77 differ only in whitespace, all from "Format sources with fbsource's
      formatters" (`303ee0b02bb`). The formatter preserved every string
      literal.
      - `c-programs/` (67): `procfs-identity-agreement`,
        `dbt-execveat-unsupported`, `get-robust-list-self`,
        `get-robust-list-thread`, `getitimer-determinism-probe`,
        `io-uring-fallback`, `io-uring-ring-determinism`, `ioctl-siocethtool`,
        `ipc-determinism`, `kcmp-eperm`, `keyctl-enosys`, `keyctl-passthrough`,
        `liteinst-advanced`, `lsm-get-self-attr-enosys`,
        `lsm-list-modules-enosys`, `lsm-set-self-attr-enosys`,
        `meminfo-available-deterministic`, `meminfo-cached-deterministic`,
        `meminfo-free-deterministic`, `perf-event-hardware-enosys`,
        `perf-event-open-enosys`, `perf-event-software-enosys`,
        `perf-event-watchpoint-enosys`, `pidfd-open-self`, `pidfd-poll-self`,
        `pidfd-waitid-child`, `pipe2-errno-precedence`, `prctl-dumpable`,
        `prctl-option-policy`, `proc-fdinfo`, `process-vm-readv-refusal-probe`,
        `process-vm-writev-refusal-probe`, `procfs-positioned-probe`,
        `prodcons-determinism`, `pselect6-simulation`, `ptrace-attach-eperm`,
        `ptrace-eperm`, `ptrace-seize-eperm`, `ptrace-traceme-eperm`,
        `pty-nr-count`, `record-replay-setsockopt`,
        `remap-file-pages-anonymous-enosys`, `remap-file-pages-memfd-enosys`,
        `remap-file-pages-tmpfile-enosys`, `request-key-enosys`,
        `sched-yield-progress`, `scheduler-policy-queries`,
        `setitimer-determinism`, `sigmask-preemption`,
        `sigtimedwait-timeout-0s`, `splice-enosys`, `syscall-file-io`,
        `syscall-file-metadata`, `syscall-quick-wins`, `sysinfo`,
        `sysinfo-uptime`, `syslog-deterministic`, `sysv-sem-enosys`,
        `sysv-shm-enosys`, `tcp-info-accept4`, `tcp-info-accept6`,
        `tcp-info-client4`, `tee-enosys`, `thread-self-procfs-handoff`,
        `thread-sync-determinism`, `ustat-enosys`, `vmsplice-enosys`.
      - `determinism-stress-c/` (8): `lock-free`, `mmap-fork-shared`,
        `pid-tid`, `pipe-chain`, `pipe-prefill`, `signal-order`,
        `thread-contention`, `thread-stress`.
      - `language-runtimes/rust-hashmap-iteration` and
        `system-utils/clock-exec-continuity`.
    - 7 differ in the order of `#include` lines and in whitespace, all from
      `303ee0b02bb`: `c-programs/mmap-stress-determinism`,
      `c-programs/record-replay-file-state-regular-sink`,
      `c-programs/signal-determinism`, `determinism-stress-c/fork-tree`,
      `determinism-stress-c/pid-tid-identity`,
      `determinism-stress/order-violation` (`tests/chaos/order_violation.c`)
      and `system-utils/startup-surface-identity`.
    - 11 are identical once comments, `#include` lines and the `_GNU_SOURCE`
      guard are ignored:
      - comments only: `c-programs/rcx-canonicalization` and
        `determinism-stress-c/producer-consumer` (`303ee0b02bb`);
      - comments only: `system-utils/file-timestamp-identity` and
        `system-utils/shm-coherency-identity` (`303ee0b02bb` and
        `34462d7afbc`);
      - comments and includes: `system-utils/errno-path-identity`
        (`303ee0b02bb`);
      - comments, includes and the guard: `c-programs/madvise-determinism`,
        `c-programs/ppoll-readv`, `c-programs/ppoll-simulation`,
        `c-programs/record-replay-fd-close`,
        `c-programs/recvmsg-scm-rights-mmap` and `c-programs/sigpipe-siginfo`
        (`303ee0b02bb` and "Carry the fbsource-side build, header and test
        fixes upstream", `c7779755086`).
    - 1 changes code: `c-programs/random-sources` (`tests/c/random_sources.c`),
      from `303ee0b02bb` and "Sample getrandom(2) directly in random_sources
      root-only mode" (`7a737ba17d0`, citing
      https://github.com/rrnewton/hermit/issues/3369 and
      https://github.com/rrnewton/hermit/issues/3371). The selected cell
      passes no `guest_args`, so it runs in full mode, not root-only mode, and
      its syscall path is the same as before. Its binary still differs.
  - **Scripts and directory trees.** None of the 29 shell guests changed. The
    three `examples/` scripts that `direct` cells run (`race.sh`, `date.sh`,
    `devrand.sh`) are also unchanged. Four cells still see changed inputs:
    - `determinism-stress/thread-contention` compiles
      `thread_contention.c`, `thread_stress.c` and `mmap_fork_shared.c` with
      `-g`, and all three were reformatted in `303ee0b02bb`;
    - `determinism-stress/process-chains` compiles `fork_tree.c` (include
      order) and `pipe_chain.c` (whitespace), also with `-g`;
    - `system-utils/du-tree-summary` (`du -sb -- tests/e2e`) and
      `system-utils/find-tree-metadata` (`find tests/e2e -mindepth 1
      -maxdepth 3 -type f ...`) read the `tests/e2e` tree, which changed:
      `backend-parity-c.yaml` was removed, `compat.yaml` and
      `system-utils/cat-file-read-input.txt` were added, and
      `c-programs.yaml` grew.
  - **How Hermit is built and where cells run.** "Build Hermit once per
    validation: one pinned-root build, one profile, one path"
    (`d44bbbb79ac`) has every validation consumer run one Hermit built in a
    new Cargo profile, `validate`. That profile uses release optimisation but
    keeps debug assertions and overflow checks on. Its message says a full
    validation used to compile Hermit both as a debug workspace build and as a
    release runtime build. The LiteInst runtime is still always built release.
    "Keep validation cells off the CPUs the host PMU drivers are bound to"
    (`3f66a249b30`, https://github.com/rrnewton/hermit/issues/3265) changes
    which CPUs cells run on; a PMU is the processor's performance-monitoring
    unit. Neither changes what a guest sees, but both can change the CPU and
    wall time a cell uses.

  The rest of the delta does not reach a selected cell:
  - DBT only: `095d40ed054` and `5ad890a79b4`;
  - SaBRe only: `b38a180df6a`, and the `sabre_bootstrap.rs` part of
    `c7779755086`;
  - the log-diff report and log-file format: `fdd8f55a8e7` and `5d93ff543fa`,
    which makes the epoch notice a DEBUG record;
  - CLI and container code that no selected cell exercises: `9db707b225e`,
    `f5dfe84af02`, `f7a80d5787b`, `de130c044bc`, `dc92644f96f`,
    `b30a76b9667`, `ccb041fbb6d`, `8dfceee599c` and `197d8e48016`;
  - the `cpuid` subscription, which changes only under
    `--no-virtualize-cpuid` (`01341d66633`). No selected verify cell passes
    that flag;
  - iced-x86 overflow checks (`559c56c2c2a`);
  - new manifest fields that no selected cell uses (`cec05c57042`), and
    non-blocking diagnostic cells (`fb3c2aaa311`);
  - tests (`5421f97c77e`, `1f74dee5675`, `d0d730a9e97`, `146575eaaf6`);
  - how `hermit-install` finds its pinned checkouts (`547ca9ba16e`);
  - CI.

  "Give the LiteInst activation probe an unwind-table entry" (`e4827316c7d`)
  also needs a note. Before every LiteInst run, Hermit runs its own
  32-call activation probe. This commit adds unwind-table directives to that
  probe and to `tests/c/liteinst_host_activation.c` without changing any
  instruction. Without them, the Reverie entry census described below would
  leave the probe unpatched, and every LiteInst run would stop at activation.

  `git diff --quiet b280bc4807 03bbb83581f -- ci/manifest-plan/src/timeouts.rs`
  exits 1. The diff adds 158 lines and removes 95, from `34462d7afbc`,
  `2be6440ddd6` and `18ece74f8b0`:
  - 95 calibration rows in the `LITEINST_2026_09_16` and
    `LITEINST_2026_09_17` tables are renamed from `backend-parity-c/` to
    `c-programs/`, and none of them is one of the 162;
  - new `DBT_MATRIX_2026_09_29_*` constants;
  - new `STRICT_COMPAT_FOLD_2026_10_01` constants;
  - a test.

  The `LITEINST_2026_09_27` rows and the default 22 s CPU and 57 s wall
  bounds are untouched (`git diff --quiet` on
  `tests/e2e/manifests/defaults.yaml` exits 0).

  In Reverie, the delta from `6297f715` to `dbf2b5c8` is 104 commits, all
  first-parent. The LiteInst runtime crates are no longer byte-identical:
  - `reverie-liteinst`: 60 files changed, 8,209 lines added and 567 removed;
  - `reverie-process`: 11 files changed, 1,645 added and 51 removed;
  - `reverie-preload`, `reverie-syscalls` and `reverie-memory`: unchanged
    (`git diff --quiet` exits 0 for each);
  - the five crates together: 71 files, 9,854 lines added and 618 removed.

  `reverie-liteinst`'s `Cargo.toml` adds `iced-x86` 1.21.0, an x86
  instruction decoder. Thirty-five commits touch the five crates:
  - 25 touch `reverie-liteinst`, of which 6 touch its source or
    `Cargo.toml` and 19 touch only its tests;
  - 10 touch `reverie-process`;
  - no commit touches both.

  The tracer side of LiteInst lives in `reverie-ptrace`, outside these five
  crates. It changed far more: 70 commits, 37 files, 27,066 lines added and
  213 removed. `reverie` itself changed in 3 commits (3 files, 42 lines
  added) and `safeptrace` in 3 commits (271 lines added).
  `reverie-rpc-transport` and the `liteinst2` revision `2032b49d` are
  unchanged. LiteInst does not use `reverie-e9patch`.

  The changes that can reach a LiteInst guest, by topic:
  - **Census and patching.** The entry census is, for each loaded object, the
    list of `syscall` instruction sites together with the lowest
    branch-target entry in the 64 bytes after each site. A site is patched
    only if the bytes the patch overwrites end before that entry. Seven
    changes affect it:
    - "reverie-liteinst: leave syscall sites that a branch enters on ptrace"
      (`9ecfb90fc09`, https://github.com/rrnewton/reverie/issues/812) adds
      the census. The tracer builds it from the object's unwind table. An
      object with no unwind table, or with more than one executable mapping,
      gets no census, and none of its sites is patched. A site left
      unpatched traps to the tracer on every call. The runtime handshake
      version goes from 4 to 5. The glibc functions the commit names as
      affected are `posix_madvise` and `__futex_abstimed_wait_common`.
    - "reverie-liteinst: fail closed on census read errors, skip host-mode
      images" (`e787c05c93f`, https://github.com/rrnewton/reverie/pull/818)
      makes a failed census read fail the run. It also stops building object
      images in host mode, which removes a bootstrap branch count that
      depended on the text of the memory map.
    - "reverie-liteinst: read census bytes with FOLL_FORCE, refuse objects
      whose pages fault" (`43a3fbdd1fd`).
    - "reverie-ptrace: refuse a census object only for a fault past the end of
      its file" (`827447cdf98`).
    - "seccomp: make IP_RANGE compare the end bound's low half"
      (`47be82abc34`, https://github.com/rrnewton/reverie/pull/671). Before
      this fix, syscalls whose return address fell in
      [0x71000002, 0xffffffff] ran untraced.
    - The constructor heap (https://github.com/rrnewton/reverie/issues/750):
      "Keep the LiteInst host constructor off the guest heap"
      (`2bb0e280a86`), its test (`706fd8174b3`), and "Put the LiteInst
      constructor heap in .bss and pin its out-of-memory refusal"
      (`0a857529eb6`, a 16 MiB heap). Under LiteInst, the guest's `brk` and
      heap are now untouched when `main` starts.
    - `b062e949fad` and `fc1e6cb062b` give test fixtures unwind-table
      entries.
  - **Bootstrap window.** "Expose the LiteInst runtime-bootstrap window to
    Tools and close it on failure" (`44f758e97ef`) is the Reverie half of
    https://github.com/rrnewton/hermit/issues/3338. "Clear the LiteInst
    bootstrap thread when the bootstrap window closes" (`590c3b04109`) and a
    test (`1bebc4da167`) follow it.
  - **Precise timers and single-stepping**
    (https://github.com/rrnewton/reverie/pull/665,
    https://github.com/rrnewton/reverie/issues/661):
    - "reverie-ptrace: keep a timer event across LiteInst hook traps the Tool
      does not see" (`758f1f18c01`). Before this fix, such a trap cancelled
      the timer event, and the preemption it was meant to cause was lost.
    - Related timer changes: `783a7bee779`, `5a7b41cce33`, `d7a53c62b13`,
      `59a799363c6`, `fc7846f98cf`, `63ed9c68f1a` and `527160fe530`
      (https://github.com/rrnewton/reverie/pull/694).
    - Stepping: `3a606f2da43`, `139e8ad0fe8`, `b78d533e032`
      (https://github.com/rrnewton/reverie/pull/657) and `0e600222169`.
    - "reverie-ptrace: discard the timer's overflow signal in the LiteInst
      patch helper" (`8281a50c65f`) and "discard a late timer overflow during
      syscall injection" (`1ee2cc4770c`)
      (https://github.com/rrnewton/reverie/pull/645,
      https://github.com/rrnewton/reverie/pull/678).
  - **Signal delivery and restarted syscalls.** The first five commits below
    change how the tracer handles signals and restarted syscalls around a
    syscall it injects into the guest. The sixth changes how it decides from
    `uname` whether the kernel is a PREEMPT_RT (real-time) kernel:
    - "Keep a completed injected syscall's result across a private-page stop"
      (`791009bc4dd`);
    - "Requeue signals that stop a completed injected syscall"
      (`db88909898c`);
    - "Treat io_uring_enter as a mask-swapping injected syscall"
      (`1a769c0eb82`);
    - "Leave a guest-blocked signal blocked when requeueing it after an
      injected syscall" (`2428dd2aa71`);
    - "Stop the private-page step at a held signal and detect seccomp traps"
      (`423b40e6930`);
    - "reverie-ptrace: treat a cut uname version as possibly PREEMPT_RT"
      (`2ccffb4cef0`).
  - **Fork, exec and launch.**
    - "Keep a nonleader's exec-time ECHILD from failing the ordinary run"
      (`3ef02beeacb`).
    - "Drain stale syscall/signal stops queued before the EXIT stop"
      (`1780ff311b3`).
    - "Issue the launcher's execve with its unused argument registers zeroed"
      (`2eeb704cefb`, https://github.com/rrnewton/reverie/issues/788). On
      hosted runners, the first verify run had logged `arg4: 48` where the
      second logged `0`.
    - "Protect clone stacks with mmap guard pages" (`a983eb95d5e`) and
      "Raise the cloned child's usable stack from 2 MiB to 8 MiB"
      (`895d214e495`) (https://github.com/rrnewton/reverie/issues/666,
      https://github.com/rrnewton/reverie/pull/670).
  - **vDSO.** No behaviour change was found. vDSO sites are exempt from the
    census, and `vdso.rs` only gained an aarch64 build gate (`ee4f12d74d0`).

  The rest of the Reverie delta does not reach a selected cell:
  - "Poll rare ptrace watchers and output drains only after their wakers
    fire" (`a968ae5f72e`) changes performance only;
  - the trap-only LiteInst mode (`3ac26b7e466`, `b08a6265a39`,
    `66bd9881385` and later commits) is not selected by Hermit:
    `git grep trap_only` finds nothing in `detcore/`, `detcore-model/` or
    `hermit-cli/` at `03bbb83581f`;
  - the tip `dbf2b5c8880` changes only SaBRe (`rewriter.c`);
  - `5ef758608c9` changes only DBT.

  Neither the round-2 screen at `b280bc4807` nor the 2026-09-27 census at
  Hermit `19553a64` measured the code that runs at `03bbb83581f`, for any of
  the 162 cells. Five changes reach all of them:
  - The runner now launches every LiteInst cell, and the ptrace cell it
    stands beside, from a different executable path and environment.
  - The Reverie entry census changes which `syscall` sites the runtime
    patches. Any site that a branch enters, and any object without a usable
    unwind table, now traps to the tracer on every call.
  - The LiteInst constructor no longer touches the guest heap.
  - The tracer now keeps timer events that it used to drop at hook traps, so
    preemption points can move.
  - The bootstrap window shifts every clock a LiteInst guest reads.

  On top of these, 101 cells changed in their own inputs:
  - the 96 programs above (77 whitespace, 7 include order, 11 comments, 1 code
    change in `c-programs/random-sources`), each rebuilt with `-g`;
  - the two script cells `determinism-stress/thread-contention` and
    `determinism-stress/process-chains`;
  - the two tree-reading cells `system-utils/du-tree-summary` and
    `system-utils/find-tree-metadata`;
  - `c-programs/listmount-enosys`, whose recipe changed but whose source did
    not. It is one of the eight recipe changes from `2be6440ddd6`; the other
    seven are among the 96 programs above.

  The six cells the round-2 screen singled out
  (`system-utils/clock-exec-continuity`, `c-programs/sysinfo`,
  `c-programs/sysinfo-uptime`, `system-utils/auxv-loader-dump`,
  `c-programs/timer-create-determinism`, `system-utils/proc-uptime`) include
  the clock and uptime readers that the bootstrap-window change moves. The
  `LITEINST_2026_09_27` calibration rows are unchanged in `timeouts.rs`, but
  they were measured at `19553a64` with Reverie `b0ede531` and the old
  build. They are not evidence of a cell's CPU or wall cost under
  the `validate` profile and Reverie `dbf2b5c8`. A screen of only the 101
  changed cells would miss the harness, Detcore and Reverie changes that
  reach the other 61. A fresh screen of all 162 at `03bbb83581f` is
  therefore needed before any of them can be selected on this base. This
  document claims no result from such a screen yet.

  **The eight `expected_stdout` entries.** For each of the eight cells whose
  recipe `2be6440ddd6` changed, this change adds a `liteinst` line to
  `modes.verify.expected_stdout`, after the `dbt` line and equal to the
  `ptrace` line:
  - `c-programs/io-uring-fallback`: `io_uring blocked; epoll fallback ready`;
  - `c-programs/listmount-enosys`: `listmount deterministically unavailable`;
  - `c-programs/madvise-determinism`: `madvise-ok`;
  - `c-programs/process-vm-readv-refusal-probe`: `process-vm-readv-refused-ok`;
  - `c-programs/process-vm-writev-refusal-probe`:
    `process-vm-writev-refused-ok`;
  - `c-programs/scheduler-policy-queries`: `scheduler-policy-queries-ok`;
  - `c-programs/syscall-file-io`: `syscall-file-io-ok count=5`;
  - `c-programs/syscall-file-metadata`: `syscall-file-metadata-ok count=20`.

  Each string ends in one newline. An entry makes the cell stricter, not
  looser: without one, a LiteInst verify cell only requires its two runs to
  agree with each other, and with one, the guest's stdout must also equal the
  fixed string. All 80 census rows of these cells (ten repetitions each, in
  `qual10-batch1` to `qual10-batch4`) recorded exactly this stdout and
  outcome PASS. Six other LiteInst-enabled cells carry `ptrace` or `dbt`
  oracles without a `liteinst` one; they are outside the 162 and are left to a
  separate change.


## What was measured and how

The measurement is the census campaign retained in the parent workspace at
`ignored/validate/liteinst-groupc-19553a64`. Each of the four qualification
batches ran this command from a clean `19553a64` checkout, with a batch cells
file:

```
env -u DEV_HERMIT_PARENT ./ci/compat-envelope/pressure-test.rs run \
  --results R/qual10-batchN --cells-file R/cells-qual10-batchN.jsonl \
  --probe-disabled --backend liteinst --repetitions 10 --jobs 8 \
  --manifest-guest-cap 8 --run-timeout 21600
```

Batches 1 to 3 ran concurrently, from 06:21 to 06:54 PDT. Batch 4 ran alone,
from 06:58 to 07:25:58 PDT, which is `2026-09-27T14:25:58Z`. All four exited 0,
with walls of 1,752 s, 1,707 s, 1,725 s and 1,648 s.

The audit covered 166 cells. The 162 selected cells contribute 1,620 raw
repetitions (41 / 39 / 41 / 41 cells by batch). The independent audit
applied these checks to every repetition:

- exactly one result row, attempt 1, one inner attempt, outcome PASS, guest
  status 0 and no signal;
- only the standard strict argv. That is `--log info --strict --verify
  --verify-strict --verify-json --keep-logs`, with no `--verify-allow` and
  `relaxations=[]`;
- the `BitwiseInfoV1` canonical comparison record, with logs and I/O buffers
  compared and no strip, ignore, skip-commit or skip-detlog;
- verdict `matched`, bitwise parity, and identical exit status, stdout and
  stderr;
- the LiteInst activation banner `[liteinst host hybrid] activation verified`
  with `traps=1` and at least 31 hooks, plus `:: Backend: LiteInst host hybrid`;
- compared INFO counts that equal the INFO lines actually present in each
  retained log, and complete log trailers (no truncation). The largest log is
  8.46 MB.

The audit read `summary.json` only as a cross-check.

| Evidence | SHA256 |
| --- | --- |
| Independent audit `REPORT.md` | `423fe524b66b6da5da0288aa6589f3587b215bbe30524dddb48c9716e279020a` |
| Audit `qualified.json` (166 cells, all 1,660 raw row paths and hashes) | `bf72064dcf9ff832c2ee03181a73edb0a1248751da58fa51d72e6ca1b930a20a` |
| Audit `timing.json` | `cda3a2c4fc7bcac1002a6e9602366cb7dd872dd70e3480d7402799179ab99504` |
| Audit `rep-facts.json` (1,820 repetition facts) | `36deea23ddaab79dd1a1e51572a5563f4ee9e30f0142f56dc96aecfdc42e471e` |
| Census verifier `REPORT.md` | `a3173e10d2d016242ed08f38c39e81dbf9862c0133e3d8a3279eb2ffabe9a390` |

The audit files are in the parent workspace's
`ignored/liteinst-lane-claude/promote-groupc/audit/`.

| Batch | `summary.json` SHA256 | `run.json` SHA256 | Cells file SHA256 | Executed Hermit ELF SHA256 |
| --- | --- | --- | --- | --- |
| qual10-batch1 | `d6978128c7c31bb7d846d0ae4b25ef48b186bc382ab303b938eaef29e2b2f168` | `96a3400a234d6bc2467bb9147b1a9c45020be0800de44f2accd85b770a5b9cdb` | `c7c4ba9e5037d08e268acf8ff5514692712e85afa13f76bd85ce37e1e4ed600e` | `282b40dba9988de005741af861f80961cc6fb89e5dc7efc3b2e50e5db9e8fd64` |
| qual10-batch2 | `a7bf69a54c3db16d0282e54674b08769afa1761dc0d94f55d6383d90be113be6` | `cee3432eb63258b0c486edb3be07fa8e2537c8f1bb01107b0d529349622d1a64` | `ba3dc200c9d3ec1ecf8799c01d2227ed1d8398bf7c531e2a2aa4ea4933853182` | `6f62d6b54896d4ddc980c911fd5d24d8bac756eb7b5d923eec8e759811ef2c1b` |
| qual10-batch3 | `faf5e064801c7398555ab7911140b25d5bdd3928311fbd1e366ccb7dba0fffbe` | `7a9cb19cadb81501733a3f64f4462a87bc15ffefa452a7a763257e32197d5b34` | `45cf902dd3c4175e9b8fe9a01960b6419753e558035715b108b3e2441284e676` | `bd0bce7d0dbf02d5fb1de040c28bc9e3474790bcc53809ec15f2f1a9c52a0924` |
| qual10-batch4 | `43a8d7af1cd68584f5448b6b2fd69d9a7915f6ccfed110a7e2320e3c3d1532a1` | `5b6403c55b899d17917bc0b703432d14e5221c09e1ba2c6dcf31a720e000904b` | `014c198479ea9561a9e462a1226ece8be08519b4ab79ca60ac7490d5dec0b35b` | `633d05a23f572678eccb565cabdd8e9573410336fd4d658fdaa516bceb9eeb94` |

The four ELF hashes differ because each batch built the same clean source
into its own directory. The audit records the per-repetition binary hash.

## Counts

All counts are regenerated by the repository's own generators. `SCORECARD.md`
and `ci/compat-envelope/cells.json` come from `scorecard.rs update`,
`ci/expected-e2e-plan.json` from `test-harness expected-plan`, and
`ci/dag/validate.json` from `generate-validation-dag --write`.

| Quantity | Before (03bbb83581f) | After |
| --- | ---: | ---: |
| LiteInst `verify`: selected / enabled but unselected / disabled (of 563) | 146 / 3 / 414 | 308 / 3 / 252 |
| LiteInst, all modes: selected / not selected / not applicable (of 1,689) | 146 / 3 / 1,540 | 308 / 3 / 1,378 |
| Comparable cells selected by full (of 9,008) | 1,084 | 1,246 |
| Enabled but unselected comparable cells | 148 | 148 |
| Not-applicable comparable cells | 7,776 | 7,614 |
| Required full-plan cells (including 5 custom commands) | 1,089 | 1,251 |
| Hosted-portable plan cells | 1,082 | 1,244 |
| DAG `result_manifests` entries over all steps (LiteInst `verify` among them) | 2,600 (293) | 2,924 (617) |

These are selection counts, not a backend determinism percentage. The
denominator is unchanged, so the percentages in `SCORECARD.md` are comparable
across this change. All other backends and modes keep their selection. The 3
enabled-but-unselected LiteInst cells and the 252 still-disabled LiteInst
`verify` cells (including `system-utils/sort-random`,
`c-programs/environment-and-workdir`, `c-programs/pipe-multiwriter-ordering`
and `language-runtimes/perl-io-subprocess-time` above) are outside this change.

At round 2's base `b280bc4807` (3 custom commands) the same rows were 146 / 3 / 212 of 361, 146 /
3 / 934 of 1,083, 856 of 5,776, 150, 4,770, 859, 855 and 2,173 (293), and the
portable `backend-parity-c` node held 173 non-ptrace `verify` cells, 97 of
them LiteInst. Main has since added 202 tests to the cell table (189 in the
strict compatibility corpus `compat.yaml`, 8 in `c-programs` and 5 in
`system-utils`; 361 tests became 563) and folded `backend-parity-c` into
`c-programs`, so that row no longer exists.
Both bases gain the same 162 cells, and the DAG gains 324 entries, two per
cell.

## Bounds are unchanged

The ordinary LiteInst bounds stay **22 CPU / 57 wall seconds**, with scale
multipliers of 1.0 and 3 GiB of portable memory. No selected cell has a
per-backend timeout override. The per-log cap, aggregate log quota,
comparison policy, retry limit and admission rules are untouched.

In `ci/dag/validate.json` only the `result_manifests` of fourteen existing
manifest nodes change: seven portable and seven on-host, adding two result
owners per cell (324 entries for 162 cells). Their commands, wall and CPU node
bounds, resources and hints are byte-identical. The CPU bound is 7,200 s for
all fourteen; the wall bound is 900 s for the two `c-programs` nodes (raised
from 600 s by `34462d7afbc` when `backend-parity-c` was folded in) and 600 s
for the other twelve. The added per-node work is the sum of the cells' p90
walls:

| Node | Recent full-validate wall | Added p90 wall | Workers |
| --- | ---: | ---: | --- |
| `system-utils` | 38 to 40 s | 102.2 s | 1 (serial) |
| `c-programs` | 52 to 57 s | 278.1 s | 8 |
| `language-runtimes` | 21 to 33 s | 60.2 s | 1 |
| `determinism-stress-c` | 14 to 16 s | 30.6 s | 1 |
| `determinism-stress` | 12 to 21 s | 26.5 s | 1 |
| `data-handling` | 13 to 17 s | 17.8 s | 1 |
| `chaos-c` | 7 to 8 s | 2.3 s | 1 |

The recent walls come from the four newest full validates in the parent
workspace's `ignored/validate/validate-full-*.log`, at heads `1139c661ede3`,
`d0deac95dad1`, `b40707ad6be0` and `a3074162c6ac`. Two are `main` commits
three and one commits before `03bbb83581f` (`1139c661ede3`, `b40707ad6be0`);
the other two are pull-request heads one commit on top of `b40707ad6be0` and
of `03bbb83581f` itself. All seven nodes passed in all four. These walls are
much lower than round 2's (110 to 119 s for `system-utils` and `c-programs`)
because validation now runs one Hermit built in the optimised `validate`
profile (`d44bbbb79ac`). The added p90 walls are the calibration's, measured
at `19553a64` with the older build, so they probably overstate the added work.
The largest serial node is estimated at about 142 s (40 + 102.2) against its
unchanged 600 s bound. This is an estimate, not a measured validate of this
change; the fresh screen at `03bbb83581f` remeasures every cell's wall.

## Per-cell timeout calibration

Every row has `mode=verify`, `backend=liteinst`, `lane=portable` and ten
samples:

- CPU is the raw row's aggregate `cpu_usage_usec`.
- Wall is its complete `duration_ms`, including preparation.
- The nearest-rank p90 is the ninth of ten independently sorted values.
- The existing formula is `ceil(1.5 × p90 CPU)` and `ceil(4 × p90 wall)`,
  using threefold wall only where the fourfold value exceeds 120 seconds.
- No outlier is discarded.

The largest derived bounds come from `system-utils/auxv-loader-dump`:
14 CPU / 51 wall seconds at p90, and 15 / 53 using the maximum samples. Both
are within the unchanged 22/57. The new dated array
`LITEINST_2026_09_27_TIMEOUT_CALIBRATIONS` in
`ci/manifest-plan/src/timeouts.rs` records these rows. It extends the existing
formula and required-selection tests without rewriting the frozen census or
the earlier 2026-09-16 and 2026-09-17 arrays.

The array is a `static` rather than a `const`. At 162 rows of 104 bytes
(16,848 bytes) it exceeds Clippy's 16 KiB `large_const_arrays` threshold, and
`static` is Clippy's own suggested fix; no lint is allowed. Newly enabled
cells add equally to enabled and required, so the enabled-but-unselected count does not move.

| Test | Batch | p90 CPU (µs) | p90 wall (ms) | Derived CPU/wall (s) | Max-sample derived (s) |
| --- | --- | ---: | ---: | ---: | ---: |
| c-programs/dbt-execveat-unsupported | qual10-batch3 | 1,529,430 | 3,215 | 3/13 | 3/15 |
| c-programs/get-robust-list-self | qual10-batch4 | 941,004 | 2,193 | 2/9 | 2/9 |
| c-programs/get-robust-list-thread | qual10-batch1 | 1,117,813 | 2,602 | 2/11 | 3/13 |
| c-programs/getcpu | qual10-batch2 | 985,855 | 2,339 | 2/10 | 2/11 |
| c-programs/getitimer-determinism-probe | qual10-batch3 | 1,031,989 | 2,339 | 2/10 | 2/12 |
| c-programs/getsockopt-null | qual10-batch4 | 972,188 | 2,244 | 2/9 | 2/10 |
| c-programs/hello-alarm | qual10-batch1 | 1,162,637 | 2,871 | 2/12 | 2/13 |
| c-programs/hello-signals | qual10-batch2 | 994,518 | 2,348 | 2/10 | 2/10 |
| c-programs/io-uring-fallback | qual10-batch3 | 1,013,200 | 2,451 | 2/10 | 2/11 |
| c-programs/io-uring-ring-determinism | qual10-batch4 | 963,301 | 2,185 | 2/9 | 2/9 |
| c-programs/ioctl-siocethtool | qual10-batch1 | 1,069,616 | 2,635 | 2/11 | 2/11 |
| c-programs/ipc-determinism | qual10-batch2 | 1,187,537 | 2,517 | 2/11 | 2/11 |
| c-programs/just-spin | qual10-batch3 | 1,180,157 | 2,808 | 2/12 | 2/12 |
| c-programs/kcmp-eperm | qual10-batch4 | 1,058,882 | 2,321 | 2/10 | 2/10 |
| c-programs/keyctl-enosys | qual10-batch1 | 1,119,633 | 2,616 | 2/11 | 2/12 |
| c-programs/keyctl-passthrough | qual10-batch2 | 990,235 | 2,355 | 2/10 | 2/10 |
| c-programs/listmount-enosys | qual10-batch3 | 1,018,697 | 2,355 | 2/10 | 2/11 |
| c-programs/liteinst-advanced | qual10-batch4 | 3,947,061 | 5,712 | 6/23 | 7/23 |
| c-programs/lsm-get-self-attr-enosys | qual10-batch1 | 1,113,796 | 2,614 | 2/11 | 2/12 |
| c-programs/lsm-list-modules-enosys | qual10-batch2 | 1,046,199 | 2,381 | 2/10 | 2/10 |
| c-programs/lsm-set-self-attr-enosys | qual10-batch3 | 981,272 | 2,397 | 2/10 | 2/10 |
| c-programs/madvise-determinism | qual10-batch4 | 938,455 | 2,161 | 2/9 | 2/9 |
| c-programs/map-shadow-stack-enosys | qual10-batch1 | 1,097,466 | 2,689 | 2/11 | 2/11 |
| c-programs/memfd-secret-enosys | qual10-batch2 | 1,022,694 | 2,493 | 2/10 | 2/11 |
| c-programs/meminfo-available-deterministic | qual10-batch3 | 1,120,757 | 2,435 | 2/10 | 2/10 |
| c-programs/meminfo-cached-deterministic | qual10-batch4 | 984,223 | 2,220 | 2/9 | 2/9 |
| c-programs/meminfo-free-deterministic | qual10-batch1 | 1,122,742 | 2,672 | 2/11 | 2/12 |
| c-programs/memorypress | qual10-batch2 | 1,174,522 | 2,615 | 2/11 | 4/15 |
| c-programs/mmap-stress-determinism | qual10-batch3 | 1,228,950 | 2,554 | 2/11 | 2/11 |
| c-programs/nanosleep-par | qual10-batch4 | 1,231,030 | 2,461 | 2/10 | 3/12 |
| c-programs/nanosleep-threads-nocrash | qual10-batch1 | 1,115,339 | 2,510 | 2/11 | 2/12 |
| c-programs/netns-cookie-tcp4 | qual10-batch3 | 1,059,689 | 2,448 | 2/10 | 2/11 |
| c-programs/netns-cookie-tcp6 | qual10-batch4 | 945,225 | 2,222 | 2/9 | 2/9 |
| c-programs/netns-cookie-udp4 | qual10-batch1 | 1,042,975 | 2,634 | 2/11 | 2/11 |
| c-programs/perf-event-hardware-enosys | qual10-batch2 | 1,127,850 | 2,520 | 2/11 | 2/11 |
| c-programs/perf-event-open-enosys | qual10-batch3 | 1,040,433 | 2,513 | 2/11 | 3/13 |
| c-programs/perf-event-software-enosys | qual10-batch4 | 996,385 | 2,300 | 2/10 | 2/10 |
| c-programs/perf-event-watchpoint-enosys | qual10-batch1 | 1,034,246 | 2,477 | 2/10 | 2/11 |
| c-programs/periodic-setitimer-delivery | qual10-batch2 | 1,032,672 | 2,392 | 2/10 | 2/10 |
| c-programs/pidfd-open-self | qual10-batch3 | 1,068,035 | 2,509 | 2/11 | 2/11 |
| c-programs/pidfd-poll-self | qual10-batch4 | 957,364 | 2,220 | 2/9 | 2/10 |
| c-programs/pidfd-waitid-child | qual10-batch1 | 1,004,218 | 2,415 | 2/10 | 2/10 |
| c-programs/pipe2-errno-precedence | qual10-batch2 | 1,059,642 | 2,398 | 2/10 | 2/10 |
| c-programs/ppoll-readv | qual10-batch3 | 1,006,168 | 2,399 | 2/10 | 2/10 |
| c-programs/ppoll-simulation | qual10-batch4 | 1,044,383 | 2,308 | 2/10 | 2/10 |
| c-programs/prctl-dumpable | qual10-batch1 | 1,092,794 | 2,570 | 2/11 | 2/11 |
| c-programs/prctl-option-policy | qual10-batch2 | 1,111,542 | 2,484 | 2/10 | 2/11 |
| c-programs/print-memaddrs | qual10-batch3 | 961,039 | 2,329 | 2/10 | 2/10 |
| c-programs/printf-with-threads | qual10-batch4 | 956,484 | 2,247 | 2/9 | 3/14 |
| c-programs/proc-fdinfo | qual10-batch1 | 1,095,766 | 2,805 | 2/12 | 2/12 |
| c-programs/process-mrelease-enosys | qual10-batch3 | 1,029,910 | 2,357 | 2/10 | 3/14 |
| c-programs/process-vm-readv-refusal-probe | qual10-batch4 | 935,507 | 2,178 | 2/9 | 2/9 |
| c-programs/process-vm-writev-refusal-probe | qual10-batch1 | 1,142,926 | 2,720 | 2/11 | 3/15 |
| c-programs/procfs-identity-agreement | qual10-batch2 | 1,166,073 | 2,737 | 2/11 | 2/13 |
| c-programs/procfs-positioned-probe | qual10-batch3 | 1,086,051 | 2,414 | 2/10 | 2/10 |
| c-programs/prodcons-determinism | qual10-batch4 | 3,887,412 | 5,817 | 6/24 | 6/24 |
| c-programs/pselect6-simulation | qual10-batch1 | 1,289,622 | 2,632 | 2/11 | 3/11 |
| c-programs/ptrace-attach-eperm | qual10-batch2 | 1,147,426 | 2,771 | 2/12 | 2/12 |
| c-programs/ptrace-eperm | qual10-batch3 | 990,754 | 2,385 | 2/10 | 2/10 |
| c-programs/ptrace-seize-eperm | qual10-batch4 | 1,010,424 | 2,334 | 2/10 | 2/10 |
| c-programs/ptrace-traceme-eperm | qual10-batch1 | 1,086,725 | 2,515 | 2/11 | 2/12 |
| c-programs/pty-nr-count | qual10-batch2 | 1,114,402 | 2,610 | 2/11 | 2/11 |
| c-programs/random-sources | qual10-batch3 | 1,084,485 | 2,466 | 2/10 | 2/10 |
| c-programs/rcx-canonicalization | qual10-batch4 | 1,032,703 | 2,317 | 2/10 | 2/10 |
| c-programs/record-replay-fd-close | qual10-batch1 | 1,584,215 | 3,039 | 3/13 | 3/13 |
| c-programs/record-replay-file-state-regular-sink | qual10-batch3 | 1,081,828 | 2,451 | 2/10 | 2/10 |
| c-programs/record-replay-lseek-seek-cur | qual10-batch4 | 1,131,037 | 2,432 | 2/10 | 2/10 |
| c-programs/record-replay-setsockopt | qual10-batch1 | 1,053,911 | 2,527 | 2/11 | 2/11 |
| c-programs/recvmsg-scm-rights-mmap | qual10-batch2 | 1,154,899 | 2,935 | 2/12 | 4/15 |
| c-programs/remap-file-pages-anonymous-enosys | qual10-batch3 | 1,243,631 | 2,611 | 2/11 | 3/14 |
| c-programs/remap-file-pages-memfd-enosys | qual10-batch4 | 993,407 | 2,258 | 2/10 | 2/10 |
| c-programs/remap-file-pages-tmpfile-enosys | qual10-batch1 | 1,068,679 | 2,462 | 2/10 | 2/11 |
| c-programs/request-key-enosys | qual10-batch2 | 1,014,711 | 2,691 | 2/11 | 3/15 |
| c-programs/sched-setattr-batch | qual10-batch3 | 938,491 | 2,283 | 2/10 | 2/10 |
| c-programs/sched-setattr-idle | qual10-batch4 | 937,936 | 2,191 | 2/9 | 3/12 |
| c-programs/sched-setattr-other | qual10-batch1 | 1,080,705 | 2,390 | 2/10 | 2/12 |
| c-programs/sched-yield-progress | qual10-batch2 | 1,245,984 | 2,938 | 2/12 | 2/12 |
| c-programs/scheduler-policy-queries | qual10-batch3 | 970,113 | 2,290 | 2/10 | 2/10 |
| c-programs/setitimer-determinism | qual10-batch1 | 1,104,448 | 2,403 | 2/10 | 2/10 |
| c-programs/sigmask-preemption | qual10-batch2 | 3,304,040 | 5,179 | 5/21 | 6/22 |
| c-programs/signal-determinism | qual10-batch3 | 1,024,194 | 2,350 | 2/10 | 2/10 |
| c-programs/sigpipe-siginfo | qual10-batch4 | 1,032,976 | 2,326 | 2/10 | 2/10 |
| c-programs/sigtimedwait-no-timeout | qual10-batch1 | 1,062,766 | 2,550 | 2/11 | 2/11 |
| c-programs/sigtimedwait-timeout-0s | qual10-batch2 | 1,129,481 | 2,640 | 2/11 | 2/12 |
| c-programs/sigtimedwait-timeout-1s | qual10-batch3 | 1,033,711 | 2,579 | 2/11 | 2/11 |
| c-programs/splice-enosys | qual10-batch4 | 950,926 | 2,232 | 2/9 | 2/9 |
| c-programs/statmount-enosys | qual10-batch1 | 966,629 | 2,267 | 2/10 | 2/10 |
| c-programs/syscall-file-io | qual10-batch2 | 1,101,910 | 2,453 | 2/10 | 2/11 |
| c-programs/syscall-file-metadata | qual10-batch3 | 1,031,059 | 2,342 | 2/10 | 2/10 |
| c-programs/syscall-quick-wins | qual10-batch4 | 1,003,522 | 2,276 | 2/10 | 2/10 |
| c-programs/sysfs-enosys | qual10-batch1 | 1,032,485 | 2,405 | 2/10 | 2/10 |
| c-programs/sysinfo | qual10-batch2 | 1,064,538 | 2,449 | 2/10 | 2/10 |
| c-programs/sysinfo-uptime | qual10-batch3 | 3,569,510 | 5,366 | 6/22 | 6/22 |
| c-programs/syslog-deterministic | qual10-batch4 | 1,041,255 | 2,292 | 2/10 | 2/10 |
| c-programs/sysv-sem-enosys | qual10-batch1 | 1,013,341 | 2,340 | 2/10 | 2/10 |
| c-programs/sysv-shm-enosys | qual10-batch2 | 1,037,996 | 2,594 | 2/11 | 2/11 |
| c-programs/tcp-info-accept4 | qual10-batch3 | 1,004,814 | 2,324 | 2/10 | 2/10 |
| c-programs/tcp-info-accept6 | qual10-batch4 | 951,266 | 2,240 | 2/9 | 2/10 |
| c-programs/tcp-info-client4 | qual10-batch1 | 1,116,605 | 2,524 | 2/11 | 4/15 |
| c-programs/tee-enosys | qual10-batch2 | 1,009,296 | 2,350 | 2/10 | 2/10 |
| c-programs/thread-self-procfs-handoff | qual10-batch3 | 1,041,851 | 2,381 | 2/10 | 2/10 |
| c-programs/thread-sync-determinism | qual10-batch4 | 1,030,113 | 2,296 | 2/10 | 2/10 |
| c-programs/threadexhaustion | qual10-batch1 | 1,103,060 | 2,786 | 2/12 | 2/12 |
| c-programs/timer-create-determinism | qual10-batch2 | 1,207,752 | 2,511 | 2/11 | 3/15 |
| c-programs/uname | qual10-batch3 | 978,654 | 2,293 | 2/10 | 2/10 |
| c-programs/ustat-enosys | qual10-batch4 | 1,074,906 | 2,310 | 2/10 | 3/14 |
| c-programs/vmsplice-enosys | qual10-batch1 | 993,039 | 2,321 | 2/10 | 2/10 |
| c-programs/wait-on-child | qual10-batch2 | 1,118,824 | 2,447 | 2/10 | 2/10 |
| chaos-c/lock-granularity | qual10-batch3 | 999,095 | 2,300 | 2/10 | 2/11 |
| data-handling/archive-roundtrip | qual10-batch4 | 7,665,123 | 10,191 | 12/41 | 12/41 |
| data-handling/jq-json-transform | qual10-batch1 | 5,154,684 | 7,585 | 8/31 | 8/34 |
| determinism-stress-c/fork-tree | qual10-batch3 | 1,433,615 | 2,898 | 3/12 | 3/12 |
| determinism-stress-c/lock-free | qual10-batch4 | 1,026,555 | 2,279 | 2/10 | 2/10 |
| determinism-stress-c/mmap-fork-shared | qual10-batch1 | 2,094,341 | 4,242 | 4/17 | 4/18 |
| determinism-stress-c/pid-tid | qual10-batch2 | 1,114,064 | 2,411 | 2/10 | 2/11 |
| determinism-stress-c/pid-tid-identity | qual10-batch3 | 1,466,114 | 2,863 | 3/12 | 3/12 |
| determinism-stress-c/pipe-chain | qual10-batch4 | 1,109,822 | 2,393 | 2/10 | 2/10 |
| determinism-stress-c/pipe-prefill | qual10-batch1 | 1,100,665 | 2,404 | 2/10 | 2/10 |
| determinism-stress-c/producer-consumer | qual10-batch2 | 1,359,009 | 2,839 | 3/12 | 3/12 |
| determinism-stress-c/signal-order | qual10-batch3 | 977,423 | 2,312 | 2/10 | 2/10 |
| determinism-stress-c/thread-contention | qual10-batch4 | 1,438,319 | 2,834 | 3/12 | 3/12 |
| determinism-stress-c/thread-stress | qual10-batch1 | 1,748,838 | 3,166 | 3/13 | 3/14 |
| determinism-stress/example-race | qual10-batch1 | 3,264,127 | 5,059 | 5/21 | 6/22 |
| determinism-stress/order-violation | qual10-batch2 | 1,000,931 | 2,306 | 2/10 | 2/10 |
| determinism-stress/process-chains | qual10-batch3 | 3,146,746 | 4,976 | 5/20 | 5/21 |
| determinism-stress/thread-contention | qual10-batch4 | 6,051,976 | 8,330 | 10/34 | 10/34 |
| determinism-stress/thread-output | qual10-batch2 | 3,856,145 | 5,871 | 6/24 | 6/24 |
| language-runtimes/bash-random | qual10-batch3 | 1,631,333 | 3,067 | 3/13 | 3/13 |
| language-runtimes/cpp-stl-determinism | qual10-batch4 | 2,370,993 | 3,866 | 4/16 | 4/17 |
| language-runtimes/gawk-random | qual10-batch1 | 3,014,594 | 4,753 | 5/20 | 5/22 |
| language-runtimes/m4-macro-mkstemp | qual10-batch2 | 4,029,368 | 6,156 | 7/25 | 7/27 |
| language-runtimes/perl-hash-order | qual10-batch4 | 2,555,677 | 4,190 | 4/17 | 5/18 |
| language-runtimes/perl-random | qual10-batch2 | 6,250,218 | 11,752 | 10/48 | 10/48 |
| language-runtimes/python-hash-determinism | qual10-batch3 | 4,633,693 | 6,622 | 7/27 | 8/27 |
| language-runtimes/python-random | qual10-batch4 | 5,019,778 | 7,105 | 8/29 | 8/29 |
| language-runtimes/ruby-random | qual10-batch1 | 3,215,384 | 5,019 | 5/21 | 5/21 |
| language-runtimes/rust-hashmap-iteration | qual10-batch2 | 1,028,450 | 2,364 | 2/10 | 2/10 |
| language-runtimes/tcl-rand-clock | qual10-batch3 | 3,552,491 | 5,319 | 6/22 | 6/22 |
| system-utils/auxv-loader-dump | qual10-batch1 | 9,323,465 | 12,647 | 14/51 | 15/53 |
| system-utils/clock-exec-continuity | qual10-batch2 | 2,155,460 | 3,736 | 4/15 | 4/16 |
| system-utils/du-tree-summary | qual10-batch3 | 1,278,645 | 2,585 | 2/11 | 2/11 |
| system-utils/errno-path-identity | qual10-batch4 | 1,071,392 | 2,316 | 2/10 | 2/11 |
| system-utils/example-date | qual10-batch1 | 2,317,328 | 4,020 | 4/17 | 4/18 |
| system-utils/example-devrand | qual10-batch2 | 2,485,701 | 4,030 | 4/17 | 4/17 |
| system-utils/file-timestamp-identity | qual10-batch3 | 1,040,508 | 2,343 | 2/10 | 2/10 |
| system-utils/find-tree-metadata | qual10-batch4 | 1,694,167 | 3,081 | 3/13 | 3/14 |
| system-utils/mcookie-random | qual10-batch2 | 2,195,732 | 3,762 | 4/16 | 8/29 |
| system-utils/mktemp-name | qual10-batch3 | 2,180,030 | 3,709 | 4/15 | 4/16 |
| system-utils/openssl-enc | qual10-batch1 | 3,812,171 | 6,270 | 6/26 | 6/26 |
| system-utils/openssl-genpkey | qual10-batch2 | 2,487,326 | 4,138 | 4/17 | 4/17 |
| system-utils/openssl-passwd | qual10-batch3 | 2,542,629 | 4,106 | 4/17 | 4/17 |
| system-utils/openssl-rand | qual10-batch4 | 2,427,811 | 3,928 | 4/16 | 4/16 |
| system-utils/openssl-x509 | qual10-batch1 | 2,615,111 | 4,473 | 4/18 | 5/19 |
| system-utils/proc-random-uuid | qual10-batch2 | 2,259,450 | 3,808 | 4/16 | 4/18 |
| system-utils/proc-uptime | qual10-batch3 | 2,136,610 | 3,692 | 4/15 | 4/15 |
| system-utils/random-device | qual10-batch4 | 2,982,041 | 4,644 | 5/19 | 5/22 |
| system-utils/shm-coherency-identity | qual10-batch1 | 1,073,061 | 2,457 | 2/10 | 2/11 |
| system-utils/shuf-permutation | qual10-batch2 | 2,158,369 | 3,900 | 4/16 | 4/17 |
| system-utils/ssh-keygen-ed25519 | qual10-batch4 | 5,714,169 | 9,872 | 9/40 | 9/40 |
| system-utils/startup-surface-identity | qual10-batch1 | 1,007,378 | 2,344 | 2/10 | 2/10 |
| system-utils/startup-tls-guards | qual10-batch2 | 1,089,633 | 2,585 | 2/11 | 2/12 |
| system-utils/uuidgen-random | qual10-batch4 | 2,299,346 | 3,769 | 4/16 | 4/17 |

## Cells held for an owner decision

These six cells passed every raw check and fit the bounds. They stay disabled
for LiteInst because their current reasons are design statements, not pending
qualification. The questions are in section 5 of the audit report.

| Cell | Current LiteInst reason | Why it is held |
| --- | --- | --- |
| applications/kvm-shell-environment (privileged) | This test specifically asserts the KVM execution path | Its ptrace verify is also disabled with the same text, so it is not a ptrace-green cell. Under LiteInst it exercises only bash startup (2,361 INFO records). |
| system-utils/harness-width-contract | The harness control is backend-independent; ptrace is the canonical required witness | It checks only a harness environment contract, and the LiteInst run is the same bash startup. |
| system-utils/nscd-neutralised | The mount is container setup and backend-independent; ptrace is the canonical required witness | It checks container setup that no backend influences. |
| system-utils/sysfs-sanitized-prefixes | The sysfs read sanitizers are backend-independent; ptrace is the canonical required witness | It would show that LiteInst read paths reach Detcore's sanitizers, which needs an owner call to override the design reason. |
| c-programs/session-identity | Session and process-group identity is unmodelled on every backend; qualify LiteInst separately rather than pinning a second passthrough | The reason explicitly asks not to pin a second passthrough. |
| determinism-stress/thread-interleaving | LiteInst coverage is owned by its backend compatibility partition | Its ptrace verify is disabled by design (chaos owns this guest) and its verify mode is `ci: false`, so it is outside the ptrace-green goal. |

Six further cells passed ten times but are excluded on timing, because a
p90-derived or maximum-derived bound exceeds 22/57:

- `c-programs/proc-locks`;
- `data-handling/shell-pipeline`;
- `data-handling/sqlite-query-determinism`;
- `data-handling/zstd-multithread`;
- `language-runtimes/bash-loop-pipe-time`;
- `language-runtimes/node-v8-jit`.

Widening a bound to admit them is not permitted. The audit's section 4 has
the samples.

## Current-source screen at the author base

The ten-repetition evidence predates the author base by one commit, so the
official pressure runner re-ran the selected cells at `b63af4583a` before
selection. The runner only accepts disabled cells through a cells file, so
the screen ran from a clean detached `b63af4583a` worktree, where these
cells are still disabled, rather than from this change. Each invocation
built its own isolated fresh checkout and full DAG (including
`gate.manifest` at 859 required cells) before running any cell:

```
env -u DEV_HERMIT_PARENT ./ci/compat-envelope/pressure-test.rs run \
  --results S/<name> --cells-file S/<cells-file> \
  --probe-disabled --backend liteinst --repetitions <R> --jobs 8 \
  --manifest-guest-cap 8 --run-timeout 21600
```

| Screen | Cells file (SHA256) | Repetitions | Exit | Wall | Result |
| --- | --- | ---: | ---: | ---: | --- |
| `screen166-r1` | `cells-166.jsonl` (`c0583f63dd5dcdcb4d446f8ef63e147940fc95c39ff6a075770e6ad6521e49f4`) | 1 | 0 | 1,588.5 s | 166 / 166 first-attempt PASS, 0 retried |
| `sysinfo4-r10` | `cells-sysinfo-4.jsonl` (`81052465f11b2593cd486ef0b25c9742ea87c108a82b88fe4821525a28a18f09`) | 10 | 0 | 1,465.3 s | 39 / 40 first-attempt PASS; `system-utils/sort-random` repetition 9 FAILED on its first attempt |

Both runs report `hermit_sha=b63af4583ad01db84a641bc3ade267f4f60e20da`,
Detcore tree `38e13c0fcc41d93f07f3df90371d01591105072d` and
`source_tree_dirty=false`. The screen's summary rows were not taken on
trust. A separate checker (`tools/check_screen.py` in the implementation
directory, SHA256
`ff47013e1ae19e2575e4bd5a3a74e022b7dbb466b02aa38cb7e9d72d70c0b401`) opened
every raw `verify-1.json`, its stderr capture and both retained logs, and
required the same properties as the qualification audit:

- attempt 1 with one inner attempt, outcome PASS and harness exit 0;
- the standard strict argv with `--backend liteinst` and no `--verify-allow`
  or `--no-strict`;
- a canonical `BitwiseInfoV1` comparison of logs and I/O buffers, stripping
  only the real wall-clock prefix;
- verdict `matched` with bitwise parity, equal nonzero compared INFO counts,
  and identical exit status, stdout and stderr;
- the `activation verified (traps=1, hooks>=31)` banner, the
  `:: Backend: LiteInst host hybrid` line, `relaxations=none` and
  `Success: deterministic`;
- compared INFO counts equal to the INFO lines present in each retained log.

It reported zero problems for all 166 rows of `screen166-r1` and for the 30
rows of the three passing sysinfo cells. For `system-utils/sort-random` it
reported 11 rows where 10 were expected, which is the failure below. As a
negative control, asking it for two repetitions per cell of the
single-repetition screen reported all 166 cells short.

In `screen166-r1` the largest per-cell cost was
`system-utils/auxv-loader-dump` at 8.83 CPU s and 12.11 wall s, inside the
unchanged 22 / 57 bound. Two cells exceeded a third of the 22 s CPU bound
(7.33 s): `system-utils/auxv-loader-dump` at 8.83 CPU s and
`data-handling/archive-roundtrip` at 7.60 CPU s. Every wall time was under a
third of the 57 s wall bound.
In `sysinfo4-r10`, `c-programs/sysinfo`, `c-programs/sysinfo-uptime` and
`system-utils/auxv-loader-dump` passed ten of ten first attempts. Their
largest costs were 1.05 / 4.00 / 10.15 CPU s and 2.56 / 5.87 / 13.38 wall s,
inside 22 / 57. `c-programs/sysinfo-uptime` ran its current guest source
here, not the stale binary measured at `19553a64`.

### Deselected after the screen: `system-utils/sort-random`

Repetition 9 of 10 failed on its first attempt. The runner's summary row
records:

```
outcome FAIL, result determinism-failure, failure_class product_failure,
reason "verification operands differ in status, stdout, or stderr"
```

Its raw `verify-1.json` records:

```
"verdict": "diverged", "bitwise_parity": false,
"compared_log_messages": {"left": 6420, "right": 6420},
stdout 165 bytes each: left sha256 7d00685fe5907e7e65e38e2e40a27b84902f7637c67d761583db5324e144ade0,
                       right sha256 1689bdf7be1836b2a7158ebe5a7c67b2366f92a3235933eab398cb6b065d2feb
"first_divergent_record": 5266, "first_divergent_syscall": 1145,
"first_divergent_scheduler_turn": 152,
"first_divergent_left_message":  "... read in fd=3 0x5555555706c0+1024->ee131e0729bd1792... chunks=256:2592698a,5e7a75b9,727f27b5,3e927fe5",
"first_divergent_right_message": "... read in fd=3 0x5555555706c0+1024->8cfca83fed370cd7... chunks=256:c2a8833c,3e5bd70f,727f27b5,3e927fe5"
```

In the retained run-1 log, fd 3 at that point is `/proc/self/maps`, opened
at syscall 1137 and not reopened before the divergence. Syscall 1145 is
the seventh read on it, of 1,024 bytes. Its first two 256-byte chunks differ
and its last two match, so the two runs saw different mapping text. The final
virtual times also differ (174,816,796 ns against 174,816,866 ns). This is
consistent with the guest-visible mapping-identity family that two original
disabled reasons name. It was not investigated further here and is not
attributed to the one-commit delta. The open issue for LiteInst
`/proc/self/maps` text that varies between runs is
https://github.com/rrnewton/hermit/issues/2397 (trampoline memfd inodes);
this divergence was not confirmed to have that cause.

The pressure runner retried that repetition itself, under its ordinary
retry policy, and the retry passed (`retried_repetitions=1`). Under this
change's rule a retried pass does not count, and nobody re-ran the cell. The
cell therefore keeps its LiteInst disabled entry, restored byte-for-byte from
`b63af4583a` ("LiteInst coverage is owned by its backend compatibility
partition"). Its calibration row is removed.

The runner deletes each fresh checkout when it finishes, but every result
row records the executed binary's content hash in `binary_sha256`. All 166
`screen166-r1` rows record
`8ac164c6b35f8a84cba4188b44f99957199813983489bd72ed7d9def015f2632`, with
e2e artifact key
`1827e78062f5286bd1375ed4b64f8929ca7d3b91ad495b92dd63e94f169f5113`. The
`sysinfo4-r10` ELF was also hashed while the run was live: SHA256
`2069b04673a871b84dda75312fbc69568d0352967d4b46154d3b554e3536a058`, artifact
key `c31e8a0ac3ef72bbcadffabefac3e030ca37dc46f2d4aeeaf0c9040315f63201`. All
41 of its rows record that same value in `binary_sha256`, so the field is a
checked content identity.

| Screen evidence | SHA256 |
| --- | --- |
| `screen166-r1/summary.json` | `8ce746bc00dba45debc62689a38f5956f0bb4e60f18a1909e75b5883fc6f8364` |
| `screen166-r1/run.json` | `ee2f762edc60b162dcadab4d083532760c56684ba792f9e4b368d1d02dfd43b2` |
| `sysinfo4-r10/summary.json` | `8f15321bf019554fd27c674fd4f3e01b6684ecb6665e38f5f6fa714368939b6f` |
| `sysinfo4-r10/run.json` | `66c6ac5c88d07e31439ed56c17371104ff4bf1a165fb6417c899b1fee82a1894` |
| `sort-random` repetition 9 attempt 1 `verify-1.json` | `dd054e86c6f8b33f124900723c66a9fdde29e85544b06cfc6b22fcb9130eed65` |

The screen results are in the parent workspace's
`ignored/liteinst-lane-claude/promote-groupc/impl/screen/`.

## Current-source screen at `b280bc4807`

The census and the author-base screen predate the 88 commits from
`e63236584625` to `b280bc4807` listed under "Base of this change", including
the rewritten `clock_exec_continuity.c` guest. The same official pressure
runner therefore re-ran the selected cells at `b280bc4807`, with the same
command as above, from a clean clone at
`b280bc4807bf1a4baff74a09e45aaf6778c09fe7`, where these cells are still
disabled. Against `b280bc4807`, this change's manifest diff touches only
selection fields (`backends_disabled` entries and `ci` membership), so the
screened guest, recipe and runtime are the ones this change selects. The two
screens ran at the same time on the same host, each with `--jobs 8`, so their
wall times include the other's load.

| Screen | Cells file (SHA256) | Repetitions | Exit | Wall | Result |
| --- | --- | ---: | ---: | ---: | --- |
| `screen162-r1` | `cells-162.jsonl` (`2e44af02e1ee3b63505b70ded2ad17d56470a2567338aedf53e6c783b39525a8`) | 1 | 0 | 1,770.6 s | 162 / 162 first-attempt PASS, 0 retried |
| `r10-6` | `cells-r10-6.jsonl` (`c6f89a9828aba0b10746145dcbe9ac0378d21e1accd8d2bef208992c6e0a2cc3`) | 10 | 0 | 1,815.6 s | 60 / 60 first-attempt PASS, 0 retried |

`cells-162.jsonl` is exactly the 162 cells this change adds to the expected
plan. `r10-6` repeats the six selected cells most exposed to the delta:

- `system-utils/clock-exec-continuity`, whose guest was rewritten;
- `c-programs/sysinfo`, `c-programs/sysinfo-uptime` and
  `system-utils/auxv-loader-dump`, the selected cells that call `sysinfo`;
- `c-programs/timer-create-determinism`, for the POSIX-timer change;
- `system-utils/proc-uptime`, for the uptime and `/proc` changes.

Both runs report `hermit_sha=b280bc4807bf1a4baff74a09e45aaf6778c09fe7`,
Detcore tree `27894a0abce8441bf62c7c17873df06fc323b5a4` (the same tree as
this change's head) and `source_tree_dirty=false`. The same checker (SHA256
`ff47013e1ae19e2575e4bd5a3a74e022b7dbb466b02aa38cb7e9d72d70c0b401`) reported
zero problems for all 162 rows of `screen162-r1` and all 60 rows of `r10-6`.
As negative controls, asking it for two repetitions per cell of
`screen162-r1` reported all 162 cells short, and asking for eleven per cell
of `r10-6` reported all six short. Summary SHA256s:
`screen162-r1/summary.json`
`66bd3df82024a5f65a6395e398059264453e8d5f059bfbf519d8e25c070cabd9`,
`r10-6/summary.json`
`9be503737c361ef75bc77f50959d61ca474f6061b27486532f974dbf9ecf4f70`.

In `screen162-r1` the largest per-cell cost was
`system-utils/auxv-loader-dump` at 8.60 CPU s and 11.50 wall s. Two cells
exceeded a third of the 22 s CPU bound: `system-utils/auxv-loader-dump` and
`data-handling/archive-roundtrip` (7.52 CPU s). Every wall time was under a
third of the 57 s wall bound. In `r10-6` the largest costs over ten
repetitions were:

| Cell | Max CPU (s) | Max wall (s) |
| --- | ---: | ---: |
| `c-programs/sysinfo` | 1.03 | 2.57 |
| `c-programs/sysinfo-uptime` | 4.02 | 6.13 |
| `c-programs/timer-create-determinism` | 1.59 | 3.03 |
| `system-utils/auxv-loader-dump` | 9.22 | 12.28 |
| `system-utils/clock-exec-continuity` | 2.25 | 4.43 |
| `system-utils/proc-uptime` | 2.26 | 3.87 |

All are inside the unchanged 22 / 57.

The calibration rows were not re-derived from this screen. They remain the
`19553a64` measurements that `LITEINST_2026_09_27_EVIDENCE_SHA` names. For
`system-utils/clock-exec-continuity` that row measured the old
single-threaded program: p90 2,155,460 CPU µs and 3,736 wall ms, giving 4 /
15 s. The rewritten program's ten `r10-6` samples give p90 2,234,738 CPU µs
and 4,362 wall ms, which the same formula turns into 4 / 18 s (4 / 18 s from
the maximum samples as well). The wall figure is higher than the recorded row
but well inside the configured 57 s. For `system-utils/auxv-loader-dump`,
the `r10-6` p90 (8,568,652 CPU µs, 11,527 wall ms) gives 13 / 47 s, inside
its recorded 14 / 51 s.

## Not selected: two former `backend-parity-c` cells

`backend-parity-c/environment-and-workdir` and
`backend-parity-c/pipe-multiwriter-ordering` passed every raw check, the
one-repetition author-base screen and the bounds. They are still not selected.
Hermit `main` commit `34462d7afbc` ("Fold the backend-parity-c manifest bucket
into c-programs") has since moved both into `c-programs.yaml` as
`c-programs/environment-and-workdir` and
`c-programs/pipe-multiwriter-ordering`, and recorded the old ids in
`retired-ids.json`.

When this change was written, both `backend-parity-c` nodes passed
`--parity-reference ptrace`. After a LiteInst candidate passed, the runner
executed a ptrace reference cell and failed the candidate when the two
backends diverged. The evidence above is LiteInst-against-LiteInst only, and
it never measured that comparison for these two cells. The comparison failed
for every LiteInst cell the node ran. The four most recent full-validate
`results.jsonl` files for `manifest_backend_parity_c` at that time (heads
`4ad1c594b825`, `694e9392a8ec` and `9c5820a6fdc3`, the last twice) each
contain the same 97 LiteInst cells with two attempts each. All 194 rows are
FAIL: 192 with "liteinst diverged from ptrace: shared Detcore INFO records"
and 2 that also differ in guest stdout.

That mechanism no longer exists at the base of this change. Hermit `main`
commit `b9ec113b5f` ("Stop requesting ptrace parity reference runs from
validation") removed the flag from both generated nodes, and
`validation_dag::assert_invariants` now refuses any step that carries it.
Commit `b280bc4807` ("Remove the ptrace parity rerun from test-harness")
deleted the rerun from the runner. Both nodes now run ordinary same-backend
verification, and a cross-backend difference is a scored parity result, not
a validation outcome (https://github.com/rrnewton/hermit/issues/3301).

The two cells stay disabled for a different reason. Enabling them would be a
new selection in `c-programs.yaml` under same-backend verification. That is a
separate decision this change does not make, and neither cell was screened at
the current base. The fold moved their LiteInst disabled entries unchanged:
at both `b280bc4807` and `03bbb83581f` each has `backends_enabled: [ptrace]`
under `modes.verify` and the same `backends_disabled.liteinst` text. This
change leaves those entries byte-for-byte as the base has them, and their
calibration rows are removed. All counts and generated files were regenerated for the final 162
cells.

## Deselected after exact-head full validation: `language-runtimes/perl-io-subprocess-time`

The census qualified this cell: ten of ten clean first-attempt strict
repetitions in `qual10-batch1`, p90 3,809,468 CPU µs and 5,717 wall ms. The
exact-head full validation of this change contradicts it. Run
`validate-liteinst-lane-claude-20260927-promote-d5d2f1eab7c1-1790541584808821677-161928-aa64acc9`
at Hermit `d5d2f1eab7c18063152483b71074d980d328aaae` failed the LiteInst
`verify` cell on both attempts. Both result rows record `outcome FAIL`,
`result crash-error`, `failure_class product_failure` and "verify exited with
status 125 before producing a terminal comparison" (3,802 ms and 3,741 ms).
The two stderr captures are byte-identical (SHA256
`45bd138a1faacc4da6110112367c69c0b97e6b07a6d41e8b4d625acc4b68c5fc`):

```
...
hermit: [liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host
:: Run1...
First run errored during --verify, not continuing to a second.
Exit status: exited with code 255
...
close child stdout: Unknown error 512 at - line 28.

HERMIT_INTERNAL_FAILURE class=cli-error
Error: First run during --verify exited with code 255
```

The first `...` stands for the `hermit: virtual-time epoch=...` line, and the
second for the lines between the exit status and the Perl error. Each
attempt's cell directory (`language-runtimes-perl-io-subprocess-time-verify-liteinst`
and `...-verify-liteinst-attempt-2`) holds a `verify-1.json` with the typed
failure record: `"verdict": "no_result"`, `"no_result_reason": {"kind":
"first_run_rejected", "exit_code": 255, ...}`, `"comparison": null` and
`"guest_exit_code": 255`. The first run was rejected, so no second run and no
comparison were produced. Line 28 of the Perl program is
`close $child_output or die "close child stdout: $!"`, the `close()` of the
pipe that carries `tr`'s output back from `IPC::Open2`. Errno 512 is
`ERESTARTSYS`, a kernel-internal restart code that must never reach a guest.
This is the known LiteInst `ERESTARTSYS` leak family named in "What this does
not establish", and its fix is not on Hermit `main`. The census's 10/10 did not
reveal it.

The run's e2e failures are otherwise the same as current `main`. The main
full validations at `4ad1c594b825` and `694e9392a8ec` each have 284 e2e cells
with no passing attempt. This run has 285: the same 284 plus this cell. The
other 162 newly selected cells all passed on their first attempt there.

The cell keeps its LiteInst disabled entry, restored byte-for-byte from the
base ("LiteInst Perl qualification is tracked by backend compatibility"), and
its calibration row is removed. It stays disabled until the `ERESTARTSYS` fix
lands and the cell is qualified again.

## Original disabled reasons

Only the listed LiteInst disabled entries are removed. Other backends'
reasons remain unchanged. Among the 162 recipes, 70 existing
per-backend `ci` maps gain `liteinst: true`; the rest already had `ci: true`.
The original reasons remain below and in the immutable base, so selection is
not read as a claim that an old failure never happened. Most are pending
qualification.

Four are capability claims the evidence contradicts. Three say "The preload
runtime cannot survive the … script's post-start exec" and
`system-utils/clock-exec-continuity` says "The preload runtime does not
survive the guest's own re-exec", yet the retained logs show
`libreverie_liteinst.so` re-opened after every `execve`.

One, `c-programs/pipe2-errno-precedence`, says "blocked by guest-visible
startup mapping identity". That failure was not reproduced in 10/10 strict
runs at `19553a64` or in the one-repetition author-base screen. This does not
contradict the reason: `system-utils/sort-random` also passed 10/10 at
`19553a64` before it diverged on a `/proc/self/maps` read (see "What this does
not establish"). The other cell carrying that reason,
`backend-parity-c/environment-and-workdir` (now
`c-programs/environment-and-workdir`), is not selected.

| Test | Original LiteInst disabled reason |
| --- | --- |
| c-programs/dbt-execveat-unsupported | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/get-robust-list-self | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/get-robust-list-thread | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/getcpu | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/getitimer-determinism-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/getsockopt-null | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/hello-alarm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/hello-signals | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/io-uring-fallback | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/io-uring-ring-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ioctl-siocethtool | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ipc-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/just-spin | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/kcmp-eperm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/keyctl-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/keyctl-passthrough | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/listmount-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/liteinst-advanced | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/lsm-get-self-attr-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/lsm-list-modules-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/lsm-set-self-attr-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/madvise-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/map-shadow-stack-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/memfd-secret-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/meminfo-available-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/meminfo-cached-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/meminfo-free-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/memorypress | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/mmap-stress-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/nanosleep-par | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/nanosleep-threads-nocrash | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/netns-cookie-tcp4 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/netns-cookie-tcp6 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/netns-cookie-udp4 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/perf-event-hardware-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/perf-event-open-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/perf-event-software-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/perf-event-watchpoint-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/periodic-setitimer-delivery | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pidfd-open-self | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pidfd-poll-self | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pidfd-waitid-child | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pipe2-errno-precedence | LiteInst canonical verification remains blocked by guest-visible startup mapping identity |
| c-programs/ppoll-readv | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ppoll-simulation | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/prctl-dumpable | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/prctl-option-policy | Only ptrace was qualified when this cell was added; qualify LiteInst separately rather than asserting it untested |
| c-programs/print-memaddrs | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/printf-with-threads | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/proc-fdinfo | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/process-mrelease-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/process-vm-readv-refusal-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/process-vm-writev-refusal-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/procfs-identity-agreement | Not yet qualified for this memfd-backed procfs identity check |
| c-programs/procfs-positioned-probe | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/prodcons-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pselect6-simulation | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ptrace-attach-eperm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ptrace-eperm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ptrace-seize-eperm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ptrace-traceme-eperm | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/pty-nr-count | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/random-sources | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/rcx-canonicalization | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/record-replay-fd-close | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/record-replay-file-state-regular-sink | Regular-file sendfile variant has not been qualified on LiteInst |
| c-programs/record-replay-lseek-seek-cur | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/record-replay-setsockopt | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/recvmsg-scm-rights-mmap | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/remap-file-pages-anonymous-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/remap-file-pages-memfd-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/remap-file-pages-tmpfile-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/request-key-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sched-setattr-batch | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sched-setattr-idle | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sched-setattr-other | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sched-yield-progress | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/scheduler-policy-queries | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/setitimer-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sigmask-preemption | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/signal-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sigpipe-siginfo | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sigtimedwait-no-timeout | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sigtimedwait-timeout-0s | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sigtimedwait-timeout-1s | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/splice-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/statmount-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/syscall-file-io | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/syscall-file-metadata | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/syscall-quick-wins | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sysfs-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sysinfo | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sysinfo-uptime | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/syslog-deterministic | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sysv-sem-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/sysv-shm-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/tcp-info-accept4 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/tcp-info-accept6 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/tcp-info-client4 | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/tee-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/thread-self-procfs-handoff | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/thread-sync-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/threadexhaustion | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/timer-create-determinism | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/uname | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/ustat-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/vmsplice-enosys | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| c-programs/wait-on-child | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| chaos-c/lock-granularity | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| data-handling/archive-roundtrip | LiteInst coverage is owned by its backend compatibility partition |
| data-handling/jq-json-transform | LiteInst coverage is owned by its backend compatibility partition |
| determinism-stress-c/fork-tree | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| determinism-stress-c/lock-free | This determinism cell calibrates the ptrace strict-verify baseline; qualify LiteInst separately |
| determinism-stress-c/mmap-fork-shared | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| determinism-stress-c/pid-tid | This determinism cell calibrates the ptrace strict-verify baseline; qualify LiteInst separately |
| determinism-stress-c/pid-tid-identity | Not yet qualified for the pid-identity fixture |
| determinism-stress-c/pipe-chain | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| determinism-stress-c/pipe-prefill | This determinism cell calibrates the ptrace strict-verify baseline; qualify LiteInst separately |
| determinism-stress-c/producer-consumer | This condvar/futex determinism cell calibrates the ptrace strict-verify baseline; qualify LiteInst separately |
| determinism-stress-c/signal-order | This determinism cell calibrates the ptrace strict-verify baseline; qualify LiteInst separately |
| determinism-stress-c/thread-contention | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| determinism-stress-c/thread-stress | Initial C-corpus migration preserves the established ptrace baseline; qualify LiteInst separately |
| determinism-stress/example-race | The preload runtime cannot survive the shell script's post-start exec |
| determinism-stress/order-violation | LiteInst coverage is owned by its backend compatibility partition |
| determinism-stress/process-chains | LiteInst coverage is owned by its backend compatibility partition |
| determinism-stress/thread-contention | LiteInst coverage is owned by its backend compatibility partition |
| determinism-stress/thread-output | LiteInst coverage is owned by its backend compatibility partition |
| language-runtimes/bash-random | LiteInst Bash qualification is tracked by backend compatibility |
| language-runtimes/cpp-stl-determinism | LiteInst C++ qualification is tracked by backend compatibility |
| language-runtimes/gawk-random | LiteInst awk support is tracked by backend compatibility |
| language-runtimes/m4-macro-mkstemp | LiteInst m4 qualification is tracked by backend compatibility |
| language-runtimes/perl-hash-order | LiteInst perl support is tracked by backend compatibility |
| language-runtimes/perl-random | LiteInst Perl support is tracked by backend compatibility |
| language-runtimes/python-hash-determinism | LiteInst Python support is tracked by backend compatibility |
| language-runtimes/python-random | LiteInst Python support is tracked by backend compatibility |
| language-runtimes/ruby-random | LiteInst Ruby support is tracked by backend compatibility |
| language-runtimes/rust-hashmap-iteration | LiteInst Rust qualification is tracked by backend compatibility |
| language-runtimes/tcl-rand-clock | LiteInst Tcl qualification is tracked by backend compatibility |
| system-utils/auxv-loader-dump | Established on ptrace first; other backends ratchet against this shared entry |
| system-utils/clock-exec-continuity | The preload runtime does not survive the guest's own re-exec |
| system-utils/du-tree-summary | Qualify recursive du independently against the new ptrace golden baseline |
| system-utils/errno-path-identity | Enable once measured on this backend; ptrace is the proven baseline |
| system-utils/example-date | The preload runtime cannot survive the date script's post-start exec |
| system-utils/example-devrand | The preload runtime cannot survive the hexdump script's post-start exec |
| system-utils/file-timestamp-identity | Enable once measured on this backend; ptrace is the proven baseline |
| system-utils/find-tree-metadata | Qualify recursive find independently against the new ptrace golden baseline |
| system-utils/mcookie-random | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/mktemp-name | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/openssl-enc | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/openssl-genpkey | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/openssl-passwd | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/openssl-rand | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/openssl-x509 | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/proc-random-uuid | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/proc-uptime | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/random-device | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/shm-coherency-identity | Enable once measured on this backend; ptrace is the proven baseline. A cross-backend difference in WHICH partial state a reader catches is a FINDING, not something to stabilise away |
| system-utils/shuf-permutation | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/ssh-keygen-ed25519 | LiteInst coverage is owned by its backend compatibility partition |
| system-utils/startup-surface-identity | Enable once measured on this backend; ptrace is the proven baseline. A cross-backend difference in the RAW vDSO base or AT_RANDOM is a FINDING to report, never something to normalise away |
| system-utils/startup-tls-guards | Qualify the LiteInst startup TLS path independently after the ptrace and SaBRe ownership boundary is fixed |
| system-utils/uuidgen-random | LiteInst coverage is owned by its backend compatibility partition |

## What this does not establish

- **Host hybrid only.** Every run is the activated LiteInst host hybrid with
  the ptrace Detcore Tool. None of it is in-process LiteInst or ptrace-free
  execution, and it says nothing about lower overhead.
- **Same-backend repeats only.** These are LiteInst-against-LiteInst strict
  repeat comparisons. They are not comparisons against a ptrace golden run,
  and they are not cross-backend parity. None of the 162 cells was in the
  `backend-parity-c` node, which no longer exists at this base, and
  validation no longer runs a ptrace parity reference for any node. Bounds
  stay unchanged.
- **Ten repetitions do not bound rare divergence.** `system-utils/sort-random`
  passed ten of ten at `19553a64` and then diverged once in ten at the author
  base. At `b280bc4807`, 156 of the 162 selected cells were screened once
  each and six were screened eleven times (once plus ten), which cannot
  exclude a divergence of similar frequency. No cell has been screened at
  `03bbb83581f` yet. Full
  validation runs each selected cell once per run, so such a cell would
  appear as an intermittent red rather than be hidden.
- **Every selected cell opens the file on which `sort-random` diverged.**
  All 162 selected cells open `/proc/self/maps` between 24 and 327 times per
  run (openat records in the first retained log of each cell's first
  qualification repetition; the maximum is `system-utils/auxv-loader-dump`).
  Reads were not counted, and which component issues these opens was not
  established. `sort-random` diverged on a read of this file, so every
  selected cell opens the file behind the one known divergence site, not
  only a rare subset. See "Deselected after the screen: `system-utils/sort-random`"
  and https://github.com/rrnewton/hermit/issues/2397.
- **Historical measurement.** The ten-repetition evidence is at `19553a64`.
  The author-base screen is one repetition per cell, plus ten for the four
  sysinfo cells. The `b280bc4807` screen is one repetition for each of the
  162 selected cells, plus ten for the six cells most exposed to the 91-commit
  delta from the author base, including the rewritten
  `system-utils/clock-exec-continuity` guest. These screens bound only a gross
  regression from that delta. They do not re-derive the calibration, whose
  `system-utils/clock-exec-continuity` row still describes the old program.
  None of these screens ran at `03bbb83581f`. The 165 commits since
  `b280bc4807` change inputs of 101 of the 162 cells (see "Base of this
  change"), so the earlier screens do not stand in for one at this base.
- **Out of scope.** Nothing here covers replay, chaos, memory determinism,
  arbitrary-program determinism, Linux semantic equivalence on unsupported
  paths, or readiness to replace ptrace.
- **No green full validate.** The remaining LiteInst gaps in the census failure
  families (vfork refusal, pre-handshake executable entry, `ERESTARTSYS`
  leaking after `SIGCHLD`, and others) are unchanged. The one exact-head full
  validation, at `d5d2f1eab7c1` with 163 cells selected, failed. Its only
  failure beyond current `main`'s was `language-runtimes/perl-io-subprocess-time`,
  which is deselected above. No full validate of the 162-cell selection is
  claimed here.

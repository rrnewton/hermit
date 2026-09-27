# LiteInst qualification: 163 verify cells, 2026-09-27

This change enables and selects 163 previously disabled LiteInst `verify`
cells. Each cell has ten clean first-attempt strict repetitions from the
official pressure runner, all on the ordinary portable lane, and an independent
audit read every raw result row. Every selected cell's ptrace `verify` cell is
already selected by full validation, so this ratchets the host hybrid toward
the ptrace-green set. No cell was gained by an exemption, filter, comparator
change, bound change or manifest weakening.

The independent audit qualified 166 cells, and three of them are not
selected:

- `system-utils/sort-random` diverged in the ten-repetition current-source
  screen at the author base. The failure is quoted in the screen section.
- `backend-parity-c/environment-and-workdir` and
  `backend-parity-c/pipe-multiwriter-ordering` would run in the
  `backend-parity-c` node, which compares every LiteInst cell against a
  ptrace reference run. All 97 LiteInst cells already selected in that node
  fail that comparison in recent full validates, and these two were never
  measured under it.

Each of the three keeps its LiteInst disabled entry, restored byte-for-byte
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
  `e63236584625`, three commits after the author base. It was first rebased
  onto `55214f50e4dd`, which is `bffdf33788ca` plus one unrelated commit that
  is not on `main` and only touches the GDB helper test
  (`hermit-cli/src/bin/hermit/gdb_client.rs`); then onto `bffdf33788ca`,
  dropping that commit; then onto `e63236584625`, one commit after
  `bffdf33788ca`. None of the rebases had conflicts. The delta from the author
  base was not screened with LiteInst. It contains:
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
  (`git diff --quiet` exits 0). That the delta is inert for LiteInst is a
  reading of the source, not a measurement. `git diff --quiet b63af4583a
  e63236584625 -- tests/e2e/manifests ci/manifest-plan/src/timeouts.rs`
  exits 0.

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

The audit covered 166 cells. The 163 selected cells contribute 1,630 raw
repetitions (42 / 39 / 41 / 41 cells by batch). The independent audit
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

| Quantity | Before (e63236584625) | After |
| --- | ---: | ---: |
| LiteInst `verify`: selected / enabled but unselected / disabled (of 361) | 146 / 3 / 212 | 309 / 3 / 49 |
| LiteInst, all modes: selected / not selected / not applicable (of 1,083) | 146 / 3 / 934 | 309 / 3 / 771 |
| Comparable cells selected by full (of 5,776) | 856 | 1,019 |
| Enabled but unselected comparable cells | 150 | 150 |
| Not-applicable comparable cells | 4,770 | 4,607 |
| Required full-plan cells (including 3 custom commands) | 859 | 1,022 |
| Hosted-portable plan cells | 855 | 1,018 |
| Portable backend-parity-c relations (all / LiteInst) | 173 / 97 | 173 / 97 |
| DAG `result_manifests` entries over all steps (LiteInst `verify` among them) | 2,173 (293) | 2,499 (619) |

These are selection counts, not a backend determinism percentage. The
denominator is unchanged, so the percentages in `SCORECARD.md` are comparable
across this change. All other backends and modes keep their selection. The 3
enabled-but-unselected LiteInst cells and the 49 still-disabled LiteInst
`verify` cells (including `system-utils/sort-random` and the two
`backend-parity-c` cells above) are outside this change.

## Bounds are unchanged

The ordinary LiteInst bounds stay **22 CPU / 57 wall seconds**, with scale
multipliers of 1.0 and 3 GiB of portable memory. No selected cell has a
per-backend timeout override. The per-log cap, aggregate log quota,
comparison policy, retry limit and admission rules are untouched.

In `ci/dag/validate.json` only the `result_manifests` of fourteen existing
manifest nodes change: seven portable and seven on-host, adding two result
owners per cell. Their commands, 600 s wall and 7,200 s CPU node bounds,
resources and hints are byte-identical, and the `backend-parity-c` nodes do
not change at all. The added per-node work is the sum of the cells' p90
walls:

| Node | Recent full-validate wall | Added p90 wall | Workers |
| --- | ---: | ---: | --- |
| `system-utils` | 114 to 119 s | 102.2 s | 1 (serial) |
| `c-programs` | 110 to 115 s | 278.1 s | 8 |
| `language-runtimes` | 72 to 78 s | 65.9 s | 1 |
| `determinism-stress-c` | 37 to 40 s | 30.6 s | 1 |
| `determinism-stress` | 30 to 32 s | 26.5 s | 1 |
| `data-handling` | 51 to 54 s | 17.8 s | 1 |
| `chaos-c` | 13 to 14 s | 2.3 s | 1 |

The recent walls come from three full validates in the parent workspace's
`ignored/validate/validate-full-*.log` (heads `9c5820a6fdc3`, `a9cf326342f9`
and `dc7da377fed3`). In those runs the c-programs, backend-parity-c and
system-utils nodes reported a failure, so their walls are indicative only. The
largest serial node is estimated at about 221 s against its unchanged 600 s
bound. This is an estimate, not a measured validate of this change.

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

The array is a `static` rather than a `const`. At 163 rows of 104 bytes
(16,952 bytes) it exceeds Clippy's 16 KiB `large_const_arrays` threshold, and
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
| language-runtimes/perl-io-subprocess-time | qual10-batch1 | 3,809,468 | 5,717 | 6/23 | 6/27 |
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
disabled reasons name. It was not investigated further here, is not
attributed to the one-commit delta, and is tracked as TaskGraph task
`liteinst_maps_read_divergence`.

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

## Deselected for the ptrace parity reference: two `backend-parity-c` cells

`backend-parity-c/environment-and-workdir` and
`backend-parity-c/pipe-multiwriter-ordering` passed every raw check, both
screens and the bounds. They are still not selected. The `backend-parity-c`
node passes `--parity-reference ptrace`, so after a LiteInst candidate passes,
the runner executes a ptrace reference cell and fails the candidate when the
two backends diverge. The evidence above is LiteInst-against-LiteInst only
and never measured that comparison for these two cells.

The comparison fails for every LiteInst cell the node already runs. The four
most recent full-validate `results.jsonl` files for
`manifest_backend_parity_c` (heads `4ad1c594b825`, `694e9392a8ec` and
`9c5820a6fdc3`, the last twice) each contain the same 97 LiteInst cells with
two attempts each. All 194 rows are FAIL: 192 with "liteinst diverged from
ptrace: shared Detcore INFO records" and 2 that also differ in guest stdout.
Selecting these two cells would add two more reds of that known kind.

Both keep their LiteInst disabled entries, restored byte-for-byte from the
base, and their calibration rows are removed. All counts and generated files
were regenerated for the final 163 cells.

## Original disabled reasons

Only the listed LiteInst disabled entries are removed. Other backends'
reasons remain unchanged. Among the 163 recipes, 71 existing
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
`backend-parity-c/environment-and-workdir`, is not selected.

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
| language-runtimes/perl-io-subprocess-time | LiteInst Perl qualification is tracked by backend compatibility |
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
  and they are not cross-backend parity. No selected cell runs in the
  `backend-parity-c` node or under `--parity-reference ptrace`. Bounds stay
  unchanged.
- **Ten repetitions do not bound rare divergence.** `system-utils/sort-random`
  passed ten of ten at `19553a64` and then diverged once in ten at the author
  base. The other 160 selected cells were screened once each at the author
  base, which cannot exclude a divergence of similar frequency. Full
  validation runs each selected cell once per run, so such a cell would
  appear as an intermittent red rather than be hidden.
- **Every selected cell opens the file on which `sort-random` diverged.**
  All 163 selected cells open `/proc/self/maps` between 24 and 327 times per
  run (openat records in the first retained log of each cell's first
  qualification repetition; the maximum is `system-utils/auxv-loader-dump`).
  Reads were not counted, and which component issues these opens was not
  established. `sort-random` diverged on a read of this file, so every
  selected cell opens the file behind the one known divergence site, not
  only a rare subset. It is tracked as TaskGraph task `liteinst_maps_read_divergence`.
- **Historical measurement.** The ten-repetition evidence is at `19553a64`.
  The author-base screen is one repetition per cell, plus ten for the four
  sysinfo cells. That screen bounds only a gross regression from the one-commit
  delta. It does not re-derive the calibration, and it does not cover the
  three commits between the author base and `e63236584625`.
- **Out of scope.** Nothing here covers replay, chaos, memory determinism,
  arbitrary-program determinism, Linux semantic equivalence on unsupported
  paths, or readiness to replace ptrace.
- **No full validate.** The remaining LiteInst gaps in the census failure
  families (vfork refusal, pre-handshake executable entry, `ERESTARTSYS`
  leaking after `SIGCHLD`, and others) are unchanged. No full validate of this
  change is claimed here.

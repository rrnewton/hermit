# Syscall classification audit

Audit of every syscall previously marked `Unclassified` in `detcore/src/syscall_classification.rs`. The `Unclassified` variant is removed; every pinned x86_64 `Sysno` is now `Determinized` (handled or deterministically refused), `PassThrough` (forwarded), or `Unimplemented` (audited, determinization pending, fail-closed under `--strict`).

Counts: **Determinized 142** (128 handled + 14 ENOSYS refusal), **PassThrough 96** (74 existing + 22 audited safe), **Unimplemented 135**. Total 373.

Every syscall below already has a `Syscall classification: <name>` issue on `rrnewton/hermit` (referenced inline); no new issues were filed. Deprecated/removed syscall ENOSYS behavior was verified empirically on the reference host.

## Determinized — deterministic ENOSYS refusal (14)

Obsolete/removed/never-implemented on modern x86_64 Linux; all verified to return `ENOSYS` on the reference host. Some (`sysfs`, `uselib`, `lookup_dcookie`) are kernel-config/version dependent, so deterministic refusal makes them host-independent. `ustat` is deliberately excluded (still implemented) and stays in the backlog.

| syscall | issue |
|---|---|
| `_sysctl` | #330 |
| `afs_syscall` | #334 |
| `create_module` | #343 |
| `get_kernel_syms` | #369 |
| `getpmsg` | #375 |
| `lookup_dcookie` | #635 |
| `nfsservctl` | #312 |
| `putpmsg` | #386 |
| `query_module` | #390 |
| `security` | #421 |
| `sysfs` | #515 |
| `tuxcall` | #523 |
| `uselib` | #529 |
| `vserver` | #641 |

## PassThrough — audited safe, non-blocking, deterministic (22)

Non-blocking and repeatable under Hermit's fixed-container identity, stable filesystem, and serialized single-CPU model. Promoted so they run under `--strict` instead of hitting the fail-closed backlog.

| syscall | issue |
|---|---|
| `close_range` | #341 |
| `get_robust_list` | #371 |
| `get_thread_area` | #372 |
| `ioprio_get` | #619 |
| `ioprio_set` | #620 |
| `mlock` | #290 |
| `mlock2` | #291 |
| `mlockall` | #292 |
| `remap_file_pages` | #398 |
| `set_thread_area` | #440 |
| `setfsgid` | #442 |
| `setfsuid` | #443 |
| `setgid` | #444 |
| `setgroups` | #445 |
| `setregid` | #454 |
| `setresgid` | #458 |
| `setresuid` | #461 |
| `setreuid` | #464 |
| `setuid` | #468 |
| `shutdown` | #480 |
| `sync` | #512 |
| `syncfs` | #514 |

## Unimplemented — audited determinization backlog (135)

Potentially blocking, or exposing host/scheduler/timer/privileged state. They keep the prior fail-closed-or-forward policy (panic under `--strict`, forward otherwise) until a real deterministic handler lands. Owned by `impl-syscall-implement-batch2`. Not promoted to PassThrough (would hide nondeterminism) and not hard-ENOSYS'd (would regress programs that depend on them, e.g. container runtimes using `unshare`/`mount`/`setns`).

### Kernel modules / boot / power (privileged, container-breaking) (6)

| syscall | issue |
|---|---|
| `delete_module` | #344 |
| `finit_module` | #355 |
| `init_module` | #610 |
| `kexec_file_load` | #622 |
| `kexec_load` | #623 |
| `reboot` | #396 |

### Mount / namespace / filesystem admin (17)

| syscall | issue |
|---|---|
| `chroot` | #338 |
| `fsconfig` | #359 |
| `fsmount` | #361 |
| `fsopen` | #362 |
| `fspick` | #363 |
| `listmount` | #632 |
| `mount` | #294 |
| `mount_setattr` | #295 |
| `move_mount` | #296 |
| `open_tree` | #314 |
| `pivot_root` | #321 |
| `setns` | #448 |
| `statmount` | #481 |
| `swapoff` | #484 |
| `swapon` | #486 |
| `umount2` | #525 |
| `unshare` | #528 |

### Privileged hardware / accounting / quota / deprecated-but-present (7)

| syscall | issue |
|---|---|
| `acct` | #331 |
| `ioperm` | #617 |
| `iopl` | #618 |
| `quotactl` | #391 |
| `quotactl_fd` | #392 |
| `ustat` | #530 |
| `vhangup` | #531 |

### Time / clock administration (3)

| syscall | issue |
|---|---|
| `adjtimex` | #333 |
| `clock_adjtime` | #339 |
| `settimeofday` | #467 |

### Host identity administration (2)

| syscall | issue |
|---|---|
| `setdomainname` | #441 |
| `sethostname` | #446 |

### Security / sandboxing / keyring / bpf (11)

| syscall | issue |
|---|---|
| `add_key` | #332 |
| `bpf` | #335 |
| `keyctl` | #624 |
| `landlock_add_rule` | #626 |
| `landlock_create_ruleset` | #627 |
| `landlock_restrict_self` | #628 |
| `lsm_get_self_attr` | #638 |
| `lsm_list_modules` | #639 |
| `lsm_set_self_attr` | #279 |
| `request_key` | #403 |
| `seccomp` | #420 |

### Tracing / perf / process inspection (9)

| syscall | issue |
|---|---|
| `kcmp` | #621 |
| `perf_event_open` | #316 |
| `pidfd_getfd` | #318 |
| `pidfd_open` | #319 |
| `pidfd_send_signal` | #320 |
| `process_mrelease` | #381 |
| `process_vm_readv` | #382 |
| `process_vm_writev` | #383 |
| `ptrace` | #385 |

### Memory policy / NUMA / protection keys / special maps (14)

| syscall | issue |
|---|---|
| `get_mempolicy` | #370 |
| `map_shadow_stack` | #280 |
| `mbind` | #282 |
| `memfd_secret` | #283 |
| `migrate_pages` | #284 |
| `mincore` | #285 |
| `modify_ldt` | #293 |
| `move_pages` | #297 |
| `personality` | #317 |
| `pkey_alloc` | #322 |
| `pkey_free` | #323 |
| `pkey_mprotect` | #324 |
| `set_mempolicy` | #438 |
| `set_mempolicy_home_node` | #439 |

### File handles / device nodes / notification (8)

| syscall | issue |
|---|---|
| `cachestat` | #336 |
| `fanotify_init` | #348 |
| `fanotify_mark` | #349 |
| `mknod` | #288 |
| `mknodat` | #289 |
| `name_to_handle_at` | #311 |
| `open_by_handle_at` | #313 |
| `openat2` | #315 |

### Blocking / vectored I/O needing scheduler-aware handlers (11)

| syscall | issue |
|---|---|
| `copy_file_range` | #342 |
| `flock` | #357 |
| `preadv` | #327 |
| `preadv2` | #328 |
| `pwritev` | #388 |
| `pwritev2` | #389 |
| `readv` | #395 |
| `sendfile` | #437 |
| `splice` | #482 |
| `tee` | #517 |
| `vmsplice` | #640 |

### Async I/O (POSIX AIO) (6)

| syscall | issue |
|---|---|
| `io_cancel` | #611 |
| `io_destroy` | #612 |
| `io_getevents` | #613 |
| `io_pgetevents` | #614 |
| `io_setup` | #615 |
| `io_submit` | #616 |

### SysV / POSIX IPC (shared mem, message queues, semaphores) (18)

| syscall | issue |
|---|---|
| `mq_getsetattr` | #298 |
| `mq_notify` | #299 |
| `mq_open` | #300 |
| `mq_timedreceive` | #301 |
| `mq_timedsend` | #302 |
| `mq_unlink` | #303 |
| `msgctl` | #304 |
| `msgget` | #305 |
| `msgrcv` | #306 |
| `msgsnd` | #307 |
| `semctl` | #427 |
| `semget` | #431 |
| `semop` | #434 |
| `semtimedop` | #436 |
| `shmat` | #475 |
| `shmctl` | #476 |
| `shmdt` | #477 |
| `shmget` | #479 |

### Futex2 API (needs scheduler integration like futex) (4)

| syscall | issue |
|---|---|
| `futex_requeue` | #365 |
| `futex_wait` | #366 |
| `futex_waitv` | #367 |
| `futex_wake` | #368 |

### Readiness / signal delivery / restart (needs virtual-time/scheduler) (7)

| syscall | issue |
|---|---|
| `epoll_pwait2` | #345 |
| `recvmmsg` | #397 |
| `restart_syscall` | #404 |
| `rt_sigqueueinfo` | #407 |
| `rt_tgsigqueueinfo` | #410 |
| `select` | #424 |
| `tkill` | #520 |

### Timers / scheduler policy / kernel log (needs virtualization) (12)

| syscall | issue |
|---|---|
| `getitimer` | #604 |
| `sched_get_priority_max` | #411 |
| `sched_get_priority_min` | #412 |
| `sched_getattr` | #413 |
| `sched_getparam` | #414 |
| `sched_getscheduler` | #415 |
| `sched_rr_get_interval` | #416 |
| `sched_setattr` | #417 |
| `sched_setparam` | #418 |
| `sched_setscheduler` | #419 |
| `syslog` | #516 |
| `times` | #519 |


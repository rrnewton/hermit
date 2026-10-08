# Inherited seccomp filters as an explicit run input

Status: design. The direction was approved on 2026-10-08 and the design is in
review. It lands together with its implementation; nothing here is built yet.
Tracking issue: https://github.com/rrnewton/hermit/issues/3942.

## Background

A seccomp filter that Hermit inherits changes what a guest observes, and
nothing in a Hermit run records it today. A run inside a container and the
"same" run outside it can therefore differ. Hermit's design rule is to make
implicit inputs explicit (epoch, seeds, environment) so that a run reproduces
from its recorded configuration. This document applies that rule to inherited
seccomp filters. It also records the prototype measurements that shaped the
design. Terms used below:

- **Inherited filter**: a seccomp-BPF filter installed by whatever launched Hermit (a container runtime's default profile, a service manager's system-call restriction). Filters stack. For each call the kernel evaluates every installed filter and takes the most restrictive action, in the order KILL > TRAP > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW. A process can add filters but never remove or loosen one.
- **Hermit's filter**: the filter Reverie's ptrace backend installs in the guest. For a fail-closed run it returns `SECCOMP_RET_TRACE` for every system call, so the tracer gets a seccomp stop for each one.
- **Normal mode**: how Hermit traces today. The guest is resumed with `PTRACE_CONT`, and the tracer sees only seccomp stops.
- **Paranoid mode**: the guest is resumed with `PTRACE_SYSCALL`, so the tracer also gets a syscall-entry stop and a syscall-exit stop around every call.
- **Action map**: the action the inherited filters apply to each system-call number, measured by probing (described below).
- **Run config**: the loadable YAML file of `hermit run` options (`--config FILE`, written by `--save-config FILE`). It landed in https://github.com/rrnewton/hermit/commit/b2fb8a77af984e0793612b67c5529a9e0200af61 and already reserves the top-level `seccomp:` key, which it refuses until this design is built.

The owner's direction (2026-10-08, in the tracking issue) has five points:

1. Refuse by default when an inherited filter is present and the run config does not acknowledge it.
2. Declare the policy in the run config, as OCI/Docker-shaped rules that Hermit enforces itself, at least as strict as the inherited filter.
3. Capture the inherited filter into that section.
4. Check for drift at every startup by reading and hashing the inherited BPF.
5. Detect host interference exactly, per call, in a paranoid debugging mode.

Points 1 to 3 carry over below. Point 4 had to be replaced: the BPF cannot be read from inside, but its effect can be measured. Point 5 holds, with one exception.

## Prototype results

The prototype is a small C program, run on x86_64 under a 7.1 Linux kernel, unprivileged and as root. It installed filters on and traced only its own processes.

### Point 5 holds: the stop pattern identifies host interference exactly

Setup: the tracer installs an "inherited" filter (`getppid` returns `ERRNO(EPERM)`) and forks a child, and the child installs a Hermit-shaped filter that returns `TRACE` for every call.

| call | normal mode | paranoid mode |
| --- | --- | --- |
| `getpid` (only Hermit's filter applies) | seccomp stop (1 stop) | entry, seccomp, exit (3 stops) |
| `getppid` (the inherited ERRNO outranks TRACE) | nothing: 0 stops, so Hermit never sees the call | entry, then exit with `-1 EPERM`, and **no seccomp stop** (2 stops) |
| `setuid(0)` as non-root (EPERM from a permission check) | seccomp stop (1) | entry, seccomp, exit with `-1 EPERM` (3) |

- The syscall-entry stop comes before seccomp runs, and a call that seccomp skipped still gets an exit stop. So "an entry stop, then an exit stop with no seccomp stop between them" identifies host interference for that one call. The `setuid` row shows why the errno alone cannot: it is EPERM in both rows.
- Cost, from three rounds of 200,000 `getpid` calls on a heavily loaded machine: normal mode is 1.00 stop and 3.3 to 3.7 µs per call; paranoid mode is 3.00 stops and 9.7 to 10.9 µs per call. That is about 2.9× per traced call, matching the issue's "two extra ptrace stops".
- **Exception: kernel passthrough calls.** On x86_64, system calls 335 (`uretprobe`) and 336 (`uprobe`) are exempt from seccomp since Linux 6.13. No filter sees them, Hermit's included, and outside the uprobe trampoline they raise SIGILL. Paranoid mode must exempt these numbers, or it reports interference that is not there. The list is per architecture and per kernel version, and the design keeps it in one table.

### Point 4 does not work as written: the inherited BPF cannot be read from inside

1. `PTRACE_SECCOMP_GET_FILTER` and `PTRACE_SECCOMP_GET_METADATA` exist only on kernels built with `CONFIG_CHECKPOINT_RESTORE`. Without it, `include/linux/seccomp.h` stubs both to return `-EINVAL`. The test kernel was built without the option, and both requests returned EINVAL unprivileged and as root, whether or not the tracer was filtered. Distribution and vendor kernels differ on this option.
2. Where the option is on, `seccomp_get_filter()` and `seccomp_get_metadata()` in `kernel/seccomp.c` (checked at v6.12) return `-EACCES` unless the **calling** process has `CAP_SYS_ADMIN` **and** is not itself in seccomp mode (`current->seccomp.mode == SECCOMP_MODE_DISABLED`). A Hermit that inherited the filter is in filter mode itself, so it is refused even as root. Only an unfiltered process outside the container, with `CAP_SYS_ADMIN` and on a kernel with checkpoint/restore, can read the BPF. Capture by reading the BPF (point 3) is limited the same way.
3. Unprivileged, `/proc/<pid>/status` gives `Seccomp: 2` and `Seccomp_filters: N`. That works everywhere but is the weak fingerprint the issue already describes.

### The replacement for point 4: a measured action map, unprivileged

A throwaway child runs Hermit's TRACE-everything filter on top of whatever it inherited, and issues every system-call number with all arguments zero. The tracer resumes it with `PTRACE_SYSCALL` and classifies each number:

- **The seccomp stop arrives**: the inherited filters allowed (or only logged) the call. The tracer skips it by setting `orig_rax` to -1, so it never executes.
- **No seccomp stop arrives**: the inherited filters acted, and the call did not execute either. The exit stop gives the errno; a SIGSYS signal-delivery stop means TRAP (the tracer suppresses the signal); a SIGSYS death means KILL (respawn the child and continue from the next number).
- **Kernel passthrough numbers** (above) are not probed.

So nothing the probe asks for ever executes on the host. That holds for `exit_group`, `pause` and `rt_sigreturn` too: the probe ran past all of them to its end marker.

Measured against a test inherited filter (`uname` → EACCES, `getppid` → EPERM, `acct` → TRAP, `kexec_load` → KILL, `getuid` → LOG, and `personality(0xffffffff)` → EINVAL):

- **Coverage:** all 512 numbers classified. 506 read as allow, which includes LOG, correctly so, since a guest cannot see it. 2 errno, 1 trap, 1 kill, 2 kernel passthrough, none unclassified.
- **Cost:** 1,538 stops and one respawn, in 5.6 to 6.7 ms.
- **Fingerprint:** the hash of the map was stable across runs, and identical unprivileged and as root. Changing one errno in the inherited filter (`uname` EACCES → EPERM) changed it.
- **Core dumps must be off in the probe child.** A KILL action ends the child with SIGSYS, which dumps core. On a host whose `core_pattern` pipes cores to a user-space handler, `RLIMIT_CORE=0` does not stop the dump, and each KILL rule cost about 400 ms. Making the child non-dumpable (`prctl(PR_SET_DUMPABLE, 0)`, set before `PTRACE_TRACEME`) suppresses it, and the whole map then takes about 6 ms. The tracer can still trace a non-dumpable child it already traces.

**Limitations:**

- **Argument-conditional rules are seen only at the probed arguments.** `personality(0xffffffff)` → EINVAL read as allow, because the probe passed zero. The probe fingerprints what the host does at canonical arguments; it is not a decompilation of the filter. Rules that depend on arguments must be declared in the run config. Exact capture needs a reader outside the container (phase 5).
- **A host filter that uses USER_NOTIF cannot be probed safely.** Such a filter hands calls to a user-space supervisor, which may act on them; the issue already puts filters that fake results out of scope. The probe would give such a supervisor zero-argument calls. Detecting this case before probing is an open question (below).

## Design

### 1. Startup check (every run on a ptrace-based backend)

1. Read `Seccomp`, `Seccomp_filters` and `NoNewPrivs` from `/proc/self/status`.
   - **Mode 0:** nothing is inherited; continue.
   - **Mode 1 (strict):** refuse, since Hermit cannot run.
   - **Mode 2:** continue to step 2.
2. Probe the action map (about 6 ms) and hash it with SHA-256. Compare the hash with `seccomp.inherited.map_sha256` in the run config:
   - **No `seccomp` section:** apply the unacknowledged-filter policy below; by default, refuse (owner point 1). The remedy names the command: rerun with `--save-config FILE` to capture the section, then run with `--config FILE`.
   - **A different hash:** refuse with a "re-capture" remedy, listing each system call whose action changed, old and new.
3. **Strictness check:** every number the map shows as blocked must get an equal or stronger action from the declared rules at the probed arguments (KILL > TRAP > ERRNO > ALLOW). Otherwise refuse, naming the declared rule that is looser than the host.
4. A run config that records an inherited map but runs where nothing is inherited (mode 0) is accepted. Hermit enforces the declared rules itself, so the run reproduces outside the container, which is the purpose of declaring them.

The probe needs a ptrace child. On a host where ptrace itself is blocked, Hermit refuses before probing.

**The unacknowledged-filter policy is a single switch.** The answer to "an inherited filter is present and the run config does not acknowledge it" is one enumeration with two values, decided in exactly one function that every caller consults:

- `Error` (the default): refuse before the guest starts, as a policy refusal (exit 122), with the capture remedy above.
- `Warning`: print the same text as a warning, write it to the run's evidence, and continue. The run is then not reproducible from its config, and its `--verify` and `--run-evidence-dir` records say so.

The default is `Error` until the owner decides otherwise, and changing the default is a one-line change to that function. Only this case has a policy switch. A changed fingerprint and a declared rule looser than the host always refuse: the run config makes a claim about the host there, and the claim is false.

### 2. The `seccomp:` section of the run config

The section is shaped like the OCI/Docker seccomp profile, with one Hermit key:

```yaml
seccomp:
  defaultAction: SCMP_ACT_ALLOW
  architectures: [SCMP_ARCH_X86_64]
  syscalls:
    - names: [getppid, uname]
      action: SCMP_ACT_ERRNO
      errnoRet: 1
    - names: [personality]
      action: SCMP_ACT_ERRNO
      errnoRet: 22
      args: [{index: 0, value: 4294967295, op: SCMP_CMP_EQ}]
  inherited:                         # Hermit extension, written by capture
    probe: hermit-seccomp-probe/v1   # zero arguments; numbers 0..=511 on x86_64
    map_sha256: <hex>
    filters: 1                       # Seccomp_filters at capture; informational
```

- **Default actions:** `SCMP_ACT_ALLOW` and `SCMP_ACT_ERRNO`.
- **Rule actions:** `SCMP_ACT_ALLOW`, `SCMP_ACT_LOG` (the same as allow for the guest), `SCMP_ACT_ERRNO`, `SCMP_ACT_TRAP`, `SCMP_ACT_KILL_THREAD` and `SCMP_ACT_KILL_PROCESS`.
- **Argument operators:** OCI's `SCMP_CMP_NE`, `LT`, `LE`, `EQ`, `GE`, `GT` and `MASKED_EQ`.
- **Refused, each with a message saying why:** `SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, `listenerPath`, and filter `flags`.
- **Unknown keys** are refused, as everywhere else in the run config.

### 3. Enforcement in Detcore, not as a stacked BPF filter

Detcore's syscall dispatch matches the declared rules before any handler runs. A match returns the errno, delivers a deterministic SIGSYS with seccomp `siginfo` (TRAP), or ends the thread or process with SIGSYS (KILL).

- **Why not stack a BPF filter in the guest:** point 5 shows that an ERRNO action hides the call from Hermit. The scheduler, the DETLOG record and record/replay would all lose it. Backends that do not use seccomp at all (KVM, in-guest LiteInst, DBT) would not apply it.
- **What Detcore gives instead:** one copy shared by every backend. The result is a pure function of (system-call number, argument registers), so it is deterministic, and every match is logged in DETLOG.
- **Unchanged:** the guest's own `seccomp()` calls still return EOPNOTSUPP, as today. That is a separate question.
- **Residual exposure:** a system call that Detcore injects into the guest, or a call whose arguments the declared rules allow but the host does not, still meets the inherited filter. Paranoid mode (section 5) is the way to see that happen.

### 4. Capture

Run with `--save-config FILE` on a host where filters are inherited (mode 2). The section it writes:

- `seccomp.inherited` holds the map's SHA-256.
- `syscalls` gets one entry per errno, trap or kill result in the map, at the probed arguments.
- A warning, in the file and on stderr, says that argument-conditional host rules cannot be observed by probing and must be declared by hand.

### 5. Paranoid mode (`--seccomp-paranoid`, a debugging option)

Resume with `PTRACE_SYSCALL` and apply the point-5 test to every call, exempting the kernel passthrough table. The first interference writes a DETLOG record and refuses the run, naming the system call, its arguments and the errno. Measured cost: about 2.9× per traced call.

### Phases (each lands separately)

1. Detection, refusal and the action-map fingerprint, including the passthrough table and the non-dumpable probe child.
2. The declared `syscalls` rules enforced in Detcore, plus the strictness check.
3. Capture through `--save-config`.
4. Paranoid mode.
5. Optional: a host-side `hermit seccomp capture --pid PID`. It runs outside the container against the container's process, reads the real BPF where the kernel allows it, and emits exact argument rules (point 3 where it can work).

## Open questions

1. **Detecting a USER_NOTIF host filter before probing.** I know of no unprivileged signal that shows a filter's actions include USER_NOTIF: `/proc` exposes only the mode and filter count. Some options:
   - (a) Treat it as out of scope, as the issue does for filters that fake results, and say so in the refusal text and documentation.
   - (b) Probe a short list of harmless numbers first (`getpid`, `gettid`) and stop if any comes back without a seccomp stop yet with a success result. A supervisor answering is the only way that can happen.
   - (c) Require an explicit `seccomp.inherited.probe: skip` acknowledgement on such hosts, and fall back to the weak `/proc` fingerprint.

   (b) catches only supervisors that intercept those numbers. A recommendation would be (a) plus (b).
2. **Probe arguments.** Probing only with zeros misses argument-conditional rules. A second vector, for example all bits set, costs about 6 ms more and catches more of them. Neither is complete.
3. **Run config schema version.** Bump to `hermit-run-config/v2` when `seccomp:` becomes accepted, or keep v1 because the key only goes from refused to accepted. Today a v1 reader refuses the key, so either way an older Hermit fails safe on a newer file.

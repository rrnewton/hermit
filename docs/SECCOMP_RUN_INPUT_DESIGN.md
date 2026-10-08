# Inherited seccomp filters as an explicit run input

Status: design, revision 2. The direction was approved on 2026-10-08. Revision 1
received a design review that requested changes; this revision answers every
finding (see "Review response" at the end). It lands together with its
implementation; nothing here is built yet.
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

**What the prototype did not cover** (the review found these, and the revised design handles them):

- A USER_NOTIF supervisor that answers `CONTINUE` runs the original call without the TRACE stop ever arriving, so the probe cannot skip it.
- An inherited `ERRNO(0)` fakes success. Aimed at the probe child's own filter installation, or at the tracer's register write, it would make a call that was meant to be skipped execute instead. A return code is therefore not evidence; every step needs a positive check.
- A SIGSYS death in a single-threaded child cannot tell KILL_THREAD from KILL_PROCESS. A TRAP also carries the filter's data in `siginfo.si_errno`.

**Limitations:**

- **Argument-conditional rules are seen only at the probed arguments.** `personality(0xffffffff)` → EINVAL read as allow, because the probe passed zero. The probe fingerprints what the host does at canonical arguments; it is not a decompilation of the filter. Rules that depend on arguments must be declared in the run config. Exact capture needs a reader outside the container (phase 5).
- **A host filter that uses USER_NOTIF cannot be probed safely.** Such a filter hands calls to a user-space supervisor, which may act on them; the issue already puts filters that fake results out of scope. The probe would give such a supervisor zero-argument calls. Detecting this case before probing is an open question (below).

## The guarantee

With an inherited filter and a run config that declares a seccomp policy, the
guest observes **exactly the declared policy for every system call it makes, or
the run stops** with a refusal naming the call on which the host disagreed.
Without an inherited filter, the same declared policy gives the same guest
observations, the same Detcore accounting (syscall counts, virtual time,
happens-before anchors, DETLOG records) and the same replay events.

What makes that guarantee checkable is the per-call check in section 3, not
any startup sample. Startup observations (section 4) are early warnings and the
input to capture; they never certify that a policy is complete.

## Design

### 1. Admission at startup

Admission runs once, in the parent, before stdin is read and before either
`--verify` child exists. Its steps, in order:

1. Read `Seccomp`, `Seccomp_filters` and `NoNewPrivs` from `/proc/self/status`.
   - **Mode 0:** nothing is inherited. A declared policy is enforced in Detcore (section 3).
   - **Mode 1 (strict):** refuse; Hermit cannot run.
   - **Mode 2:** continue to step 2.
2. **The run config does not acknowledge the filter** (it has no `seccomp:` section): apply the unacknowledged-filter policy below. The default refuses **without probing**. Probing is never done on a host that has not consented to it (section 4).
3. **The run config acknowledges it:**
   - Check the transport prerequisites: ptrace is permitted, and the designated null call (section 3) is allowed by the host according to the recorded evidence.
   - If the run config records observations and does not say `probe: never`, run the startup probe as an early drift check (section 4). A difference refuses, listing old and new rows.
   - Run with entry-stop interception (section 3).

**The unacknowledged-filter policy is a single switch.** The case "an inherited filter is present and the run config does not acknowledge it" is decided by one enumeration with two values, in exactly one function that every caller consults:

- `Error` (the default): refuse before the guest starts, as a policy refusal (exit 122), with the remedies in section 7.
- `Warning`: print the same text as a warning, write it to the run's evidence, and continue. The run is then not reproducible from its config, and its `--verify` and `--run-evidence-dir` records say so.

The default stays `Error` until the owner decides otherwise, and changing it is a one-line change. No other case has a switch: a changed observation and a host that disagrees with the declared policy at runtime always refuse.

### 2. The `seccomp:` section of the run config

It is OCI/Docker-profile-shaped and typed. Its `inherited` block holds evidence about the host and is never itself a policy:

```yaml
seccomp:
  defaultAction: SCMP_ACT_ALLOW
  architectures: [SCMP_ARCH_X86_64]
  syscalls:
    - names: [getppid, uname]
      action: SCMP_ACT_ERRNO
      errnoRet: 1
      origin: observed              # Hermit extension: observed | declared | profile
    - names: [personality]
      action: SCMP_ACT_ERRNO
      errnoRet: 22
      args: [{index: 0, value: 4294967295, op: SCMP_CMP_EQ}]
      origin: declared
  inherited:                        # Hermit extension: evidence about the host
    evidence: observed              # observed (probe) | profile (operator-supplied)
    probe:
      version: hermit-seccomp-probe/v2
      arch: x86_64
      numbers: "0-511"
      argument_vectors: [zero]
      kernel_passthrough: [335, 336]   # detected on the capture host, not assumed
      null_call: getpid
    observations:                   # canonical rows; every other probed number was observed allowed
      - {nr: 63, name: uname, action: errno, errno: 13}
      - {nr: 110, name: getppid, action: errno, errno: 1}
      - {nr: 163, name: acct, action: trap, data: 0}
      - {nr: 246, name: kexec_load, action: kill_process}
    observations_sha256: <hex>      # over the canonical rows, for quick comparison only
    filters: 1                      # Seccomp_filters at capture; informational only
```

- **Rule actions:** ALLOW, LOG (the same as allow for the guest), ERRNO, TRAP (with its data), KILL_THREAD and KILL_PROCESS. Default actions: ALLOW and ERRNO.
- **Argument operators:** OCI's `NE`, `LT`, `LE`, `EQ`, `GE`, `GT` and `MASKED_EQ`.
- **Refused, each with a message:** `SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, `listenerPath`, and filter `flags`.
- **Unknown keys are refused**, as everywhere else in the run config.
- **Typed load and save.** The run config's existing loader turns option keys into command-line tokens, and a typed section cannot travel that way. The loader therefore returns the parsed `seccomp` section alongside the rewritten argv, `RunOpts` carries it, and `--save-config` writes it back unchanged. A load-save-load test pins this.
- **Recapture** replaces the `inherited` block and the `origin: observed` rules, and keeps every `origin: declared` and `origin: profile` rule.
- `--passthru-opt` (a partial syscall subscription) is refused together with a `seccomp` section, because unsubscribed calls would bypass Detcore.

### 3. Enforcement: Detcore decides once per call, at the first stop

Detcore's syscall dispatch matches the declared rules before any handler runs. A match does one of:
- returns the errno;
- delivers a deterministic SIGSYS with seccomp `siginfo` (`si_code = SYS_SECCOMP`, the call's number and architecture, and the rule's data in `si_errno`) for TRAP;
- kills the thread or the process for KILL_THREAD and KILL_PROCESS.

The result is a pure function of the system-call number and the argument registers, so it is deterministic, and every match is logged in DETLOG. A stacked BPF filter was rejected because point 5 shows that its ERRNO would hide the call from the scheduler, DETLOG and the record. Backends without seccomp (KVM, in-guest LiteInst, DBT) would not apply it at all.

**The logical event.** Each guest system call produces exactly one Detcore decision. All accounting attaches to that decision: the syscall count, the virtual-time charge, happens-before anchors, the DETLOG record and the replay record. Later ptrace stops for the same call update only transport state. So the transport can change without changing anything the guest or the log can observe.

**Mode 2: entry-stop interception.** On a host with inherited filters, Reverie resumes guest threads with `PTRACE_SYSCALL`. The prototype showed that the syscall-entry stop arrives **before** any seccomp filter runs, so Detcore decides there:

- **Declared deny:** the call must not reach the host. At the entry stop, the tracer rewrites the syscall number to the *null call*. That is a call whose number the evidence shows the host allows (`getpid` by default, checked at admission). At the exit stop it sets the declared result. The rewrite is verified at the exit stop, which must report the null call's number; anything else stops the run. The host filter never sees the denied call, so a declared deny is enforced even when the host would have done something weaker or different.
- **Declared allow:** the call continues into the filter stack.
  - If Hermit's TRACE stop arrives, the call proceeds as today, but without accounting it a second time.
  - If the exit stop arrives with no TRACE stop, and the call is not in an expected-no-TRACE context (below), the host has blocked a call the declared policy allows. The run stops with a typed refusal naming the call, its arguments and the host's result. This is where the guarantee is enforced: hidden drift, argument-conditional host rules and incomplete observations all surface here, exactly, on the first call they affect.

**Mode 0: normal transport.** On a host without inherited filters, the TRACE stop is the first stop and Detcore decides there, as today. The acceptance tests compare complete observations between the two transports.

Entry-stop interception costs two extra ptrace stops per call, measured at about 2.9× per traced call (section "Point 5 holds"). It applies only on hosts with inherited filters.

`--seccomp-paranoid` forces entry-stop interception on a mode-0 host. It replaces revision 1's paranoid mode, whose test is now always on wherever it matters. It exists to test the transport.

**Transport state machine (per thread).** Below, "entry", "exit" and "TRACE" are stops; "call" is the logical event.

| context | stops seen | TRACE expected | Detcore decision |
| --- | --- | --- | --- |
| ordinary guest call | entry, TRACE, exit | yes | at entry |
| declared deny (rewritten to the null call) | entry, TRACE (null call), exit | yes, for the null call | at entry; the null call's TRACE stop is transport only |
| `rt_sigreturn` (Hermit's filter allows it) | entry, exit | no | none, as today: Detcore does not see it |
| a call Reverie injects (from its private instruction window, which Hermit's filter allows) | entry, exit | no | none: the tracer knows it injected the call |
| a kernel passthrough number (uretprobe, uprobe) | entry, exit (or SIGILL) | no | none, as today; counted in a transport counter |
| a call interrupted by a signal | entry, [TRACE], exit with `-ERESTART*`, signal stop, then a new entry for the restart | per the restarted call | once, for the original call; the restart is transport |
| `execve` | entry, TRACE, `PTRACE_EVENT_EXEC`, exit | yes | at entry |
| the thread dies inside the call (a sibling's `exit_group`, KILL) | entry, [TRACE], `PTRACE_EVENT_EXIT` | n/a | at entry |
| `clone` | the parent's entry, TRACE and exit; the child's first stop is not a syscall stop | yes, for the parent | at the parent's entry |

The kernel passthrough list is a per-architecture, per-kernel capability. It is detected by the probe (a passthrough number gets no TRACE stop even on a mode-0 host, and raises SIGILL outside the uprobe trampoline), recorded in the evidence, and its calls are counted in a visible transport counter, never as host interference. It is not an unconditional numeric skip.

### 4. Observing the host: the safe probe

The probe runs only with consent: through the explicit capture command (section 5), or as the startup drift check of a run config that already records observations. It probes no number other than the canaries until every condition below holds. A failed condition refuses capture (exit 122) and names the condition.

1. **A sacrificial child.** It is single-threaded, in its own session and process group (so `kill(0, …)` reaches only itself), with every file descriptor closed. It also unshares user, PID, network and mount namespaces where the host allows that; this is recorded either way.
2. **Non-dumpable, checked positively.** After `PR_SET_DUMPABLE 0`, the tracer checks that `/proc/<child>/status` is owned by root, which the kernel does only for a non-dumpable task. That was measured: `uid 0` versus the tracer's own uid for a dumpable control. Without this, every KILL rule invokes the host's core handler (about 400 ms each when cores are piped to a user-space handler).
3. **Filter installation checked positively.** `Seccomp_filters` in `/proc/<child>/status` must go up by exactly one after the probe child installs Hermit's filter. A return code from `seccomp()` is not evidence, because an inherited `ERRNO(0)` can fake it.
4. **Skipping checked on canaries.** The tracer skips `getpid`, then `gettid`, at their TRACE stops (`orig_rax = -1`). Each exit stop must report `-ENOSYS` (measured: `-38`) and not a process or thread ID. That shows the tracer's register write took effect, even if an inherited `ERRNO(0)` aimed at `ptrace` returned success.
5. **No fake success and no supervisor.** Any probed number that returns without a TRACE stop and with a non-negative result aborts the probe, and the host is refused as unsupported ("a filter fakes success or a supervisor answers calls"). This catches `ERRNO(0)`, and USER_NOTIF answers other than errors, on every number the probe reaches. The canaries are probed first.
6. **Signals checked.** A SIGSYS stop counts as a TRAP only with `si_code == SYS_SECCOMP`, `si_syscall` equal to the probed number and `si_arch` equal to the probed architecture. Its `si_errno` is recorded as the TRAP's data. Any other signal aborts the probe.
7. **Kill scope resolved.** A number that kills the single-threaded child is probed again from the second thread of a two-thread child. If only that thread dies, the rule is KILL_THREAD; otherwise it is KILL_PROCESS. If neither outcome is clean, the number is left out of the captured rules and named in the output, for the operator to declare.
8. **Bounded.** Every probed number has a deadline (proposed: 100 ms), and the whole probe has one too. On expiry the child is killed and capture is refused ("a probed call blocked; a supervisor may be holding it").

**Residual risk, stated plainly.** A USER_NOTIF supervisor that answers `CONTINUE` for a number the probe reaches before step 5 can catch it runs that call in the sacrificial child, with zero arguments. Conditions 1 and 8 bound the damage; nothing removes it. That is why probing needs consent, why `probe: never` exists, and why the per-call check in section 3, not the probe, carries the guarantee.

**Evidence is sampled.** The probe observes the host only at the probed numbers, architecture and argument vectors. Two filters that differ only on a non-zero `ioctl` request produce identical observations. Revision 1 overclaimed here: observations never establish that a declared policy dominates the host, and Hermit never claims they do. Dominance is checked per call at runtime (section 3). Authoritative evidence about the whole filter comes only from an operator-supplied profile or a host-side reader (section 5).

### 5. Capture and profiles

- `hermit run --capture-seccomp --save-config FILE [OPTIONS] [-- PROGRAM ARGS]` runs admission step 1 and the safe probe (section 4). It then writes the run config with the `seccomp` section (merged as in section 2), prints a summary of the observed rows, and **exits 0 without running the guest**. This is the bootstrap that the default refusal points to.
- `--seccomp-profile PROFILE.json` imports an OCI profile, for example the container runtime's own default profile, as the declared policy, with `evidence: profile`. An imported profile is the operator's statement about the host. It is still checked per call at runtime, and if observations exist, Hermit compares the two and reports any difference.
- Later, optionally: `hermit seccomp capture --pid PID`, run on the host outside the container. Where the kernel allows it (`CONFIG_CHECKPOINT_RESTORE`, `CAP_SYS_ADMIN`, and the caller itself unfiltered), it reads the real BPF and emits exact argument rules with `evidence: profile`.

### 6. Guest-visible seccomp metadata

Detcore's procfs sanitizer passes `Seccomp_filters` through today, so a guest would see the physical stack, and a run inside a container would differ from the same run outside it. Detcore instead presents the fields from the explicit policy:

- `Seccomp: 2`.
- `Seccomp_filters`: what a guest sees today with Hermit's filter alone, plus one when the run config declares a `seccomp` section.
- `NoNewPrivs`: the value Reverie establishes.

Neither stripping the fields (which weakens verification) nor requiring the physical count to match (which contradicts outside-container reproduction) is acceptable.

### 7. Refusal messages

Each refusal says what failed and what to run next. "No acknowledgement" and "an operation the host denies" are kept apart:

- **An inherited filter that is not acknowledged** has three remedies:
  - capture it: `hermit run --capture-seccomp --save-config run.yaml …`, then `hermit run --config run.yaml` (sampled evidence);
  - import the runtime's profile with `--seccomp-profile PROFILE.json`;
  - or run in a container created without a filter (for Docker, `--security-opt seccomp=unconfined`). A filter cannot be removed from inside a running container, so the container has to be recreated.
- **Ptrace denied**, for example a container without `SYS_PTRACE`: say so separately. Granting ptrace does not remove the seccomp filter or acknowledge it.
- **Capture refused:** name the probe-safety condition that failed (section 4).

### 8. Record and replay, and `--verify`

- **Recording metadata** (which has no policy field today) gains the resolved policy and the evidence block, including the probe version.
- `hermit record` and `hermit replay` go through the same admission as `hermit run`. Replay enforces the recorded policy for the calls it executes; replayed results come from the log.
- **A replay host with inherited filters** is compared with the recording's evidence like any run config. If it differs, or the recording has no evidence, replay refuses with the capture remedy.
- **Recordings made before this change** have no policy field. They replay as today on mode-0 hosts and are refused on mode-2 hosts, unless the replay command line supplies a run config that acknowledges the filter.
- **`run --verify`:** admission runs once in the parent, and both children receive the same resolved policy and transport.

## Acceptance matrix

Every phase lands with the rows that apply to it. Each row asserts both the result and that no probed call executed where that is the point.

- **Probe safety:**
  - USER_NOTIF answering `CONTINUE`, emulated success and emulated error, `ADDFD`, a missing listener and a hung one;
  - `ERRNO(0)` on filter installation and on `ptrace`;
  - a failed non-dumpable setup.

  Assert that no probed call executes, that no core handler runs, and that cleanup completes within the deadline.
- **Signals and termination:**
  - an asynchronous SIGSYS versus a seccomp SIGSYS;
  - blocked, ignored and caught dispositions;
  - TRAP payload and registers;
  - KILL_THREAD with surviving siblings;
  - KILL_PROCESS with blocked siblings;
  - robust-futex and clear-TID cleanup.
- **Negative controls for observations:**
  - identical zero-argument observations with different argument, instruction-pointer or ABI behavior (the per-call check must catch it at runtime);
  - unprobed numbers;
  - changed TRAP data;
  - changed kill scope;
  - an extra ALLOW filter stacked on top.
- **Full-observation equality:** the same declared policy with and without inherited filters, and entry-stop versus normal transport. Compare outputs, syscall counts, continuous virtual time, happens-before anchors, INFO logs, procfs seccomp fields and replay events.
- **Transport transitions:** injection, skipping, restarts, `clone`, non-leader `exec`, fatal signals, `rt_sigreturn`, partial subscriptions (refused) and kernel passthrough numbers. Transport counters are checked separately from guest accounting.
- **Config and replay:**
  - load-save-load preservation of the typed section;
  - command-line precedence;
  - recapture that keeps declared rules;
  - drift rows that persist;
  - legacy recordings;
  - replay hosts with no, matching and changed filters.
- **Refusal UX:**
  - run the printed remedy in a default Docker container;
  - refuse before any probe, guest launch or stdin read;
  - keep unsupported capture, a missing acknowledgement and denied capabilities distinct.

## Phases (each lands separately)

1. **Admission:** the `/proc` check, refusal before any probing, the policy switch, the refusal messages and guest-visible seccomp metadata. No probe and no enforcement yet, so mode-2 hosts always refuse under `Error`.
2. **The typed `seccomp` section and Detcore enforcement on mode-0 hosts:** the logical event, TRAP and KILL semantics, recording metadata, and `--passthru-opt` refused with a section.
3. **Entry-stop interception on mode-2 hosts:** the transport state machine and the per-call runtime check. `--seccomp-paranoid` is how this is tested on mode-0 hosts.
4. **The safe probe and capture:** `--capture-seccomp`, observations, drift diagnostics and the kill-scope probe.
5. **Profiles:** `--seccomp-profile` import, and optionally the host-side BPF reader.

## Open questions

1. **Detecting a USER_NOTIF host before probing.** No unprivileged signal is known to expose it. Revision 2's answer is layered: probing only with consent, the canaries and the fake-success abort (section 4, step 5), deadlines, and the per-call runtime check as the guarantee. Is that enough, or should mode-2 hosts without an imported profile refuse capture altogether?
2. **Probe argument vectors.** A second vector (for example all bits set) costs about 6 ms and catches more argument-conditional rules. Since the runtime check now catches the rest, a single zero vector may be enough.
3. **Run config schema version.** `hermit-run-config/v2` when `seccomp:` is accepted, or stay at v1, because the key only moves from refused to accepted. A v1 reader refuses the key, so either way an older Hermit fails safe on a newer file.
4. **Cost.** Entry-stop interception costs about 2.9× per traced call on mode-2 hosts. Is that acceptable as the price of the guarantee, or should a declared-and-profiled host be allowed to opt into normal transport with a weaker, labelled guarantee?

## Review response (revision 1 review, requested changes at a4327d92)

| finding | resolution |
| --- | --- |
| 1. The probe can execute the probed call (USER_NOTIF `CONTINUE`, `ERRNO(0)`) | No probing without consent; refusal comes before probing (section 1). Positive checks for non-dumpability, filter installation and skipping; the fake-success abort; signal validation; deadlines (section 4). The residual risk is stated. |
| 2. A stronger declared rule cannot be enforced on host-hidden calls; accounting differs | Entry-stop interception decides before any filter runs. A declared deny is rewritten to the null call so it never reaches the host. One logical event carries all accounting (section 3). |
| 3. Observations prove sampled equality, not dominance | The guarantee is moved to the per-call runtime check. Observations are labelled sampled. Authoritative evidence comes only from profiles (sections 3, 4 and 5). |
| 4. Kill scope, TRAP payload, asynchronous SIGSYS | Two-thread kill-scope probe, TRAP `si_errno` recorded, SIGSYS validated, unresolved numbers left out and named (section 4). |
| 5. `Seccomp_filters` is guest-visible | The fields are presented from the explicit policy (section 6). |
| 6. Paranoid mode needs a full tracing state machine | Per-thread transport table, with exactly one logical event per call. Passthrough numbers are detected and counted visibly (section 3). |
| 7. Record and replay are unspecified | Metadata field, shared admission, replay-host comparison, legacy behavior, `--verify` (section 8). |
| 8. Capture cannot get past the refusal; typed config transport | `--capture-seccomp` exits without running the guest. Typed section carried beside the argv; load-save-load test (sections 2 and 5). |
| 9. A hash alone cannot give per-syscall drift diagnostics | Canonical observation rows plus probe coverage are stored; the hash is only a shortcut (section 2). |
| 10. Docker remediation | Separate messages for a missing acknowledgement and for denied operations. Recreating the container is stated as the only way to drop a filter (section 7). |

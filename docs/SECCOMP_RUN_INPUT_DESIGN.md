# Inherited seccomp filters as an explicit run input

Status: design, revision 3. The direction was approved on 2026-10-08. Revision 1
was reviewed and changes were requested; revision 2 answered the review with
per-call checking on every filtered host. The owner rejected that cost on
2026-10-08 ("I definitely don't want 3x ptrace stops"), so this revision makes
normal transport the design and keeps per-call checking as an opt-in paranoid
mode. It lands together with its implementation; nothing here is built yet.
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

### Why host-blocked calls cannot be classified after the fact

It is natural to expect Hermit to notice afterwards when a call failed because
of a filter. It cannot, for two separate reasons:

1. **Hermit never sees the return.** In normal transport, a call the host filter
   blocks produces no ptrace stop at all. The kernel returns the errno straight
   to the guest (measured: 0 stops for the blocked call). Hermit could see the
   return only by stopping at every syscall exit, which is the cost the owner
   rejected.
2. **The value is ambiguous anyway.** A filter returns whatever errno it chooses
   (Docker's default is EPERM), and EPERM also comes from ordinary permission
   checks. In the prototype, `setuid(0)` as non-root and a filtered `getppid`
   both returned `-1 EPERM`. They differ only in whether Hermit's TRACE stop
   arrived, which can be seen only with the extra stops.

What Hermit can do cheaply is control an experiment: the startup probe issues
calls it chose itself and checks for its own TRACE stop on each one. That is
why drift detection happens at startup, against recorded observations, and not
during the run.

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

## The guarantee, and its limits

Exact reproducibility is the point of every Hermit run. `--verify` and
`--verify-strict` are only testing features that check it by running twice;
nothing in this design depends on whether a run uses them. Every guarantee and
every limit below applies to an ordinary `hermit run`.

With an inherited filter, Hermit runs only when the run config acknowledges it.
At startup the host's measured actions must equal the declared policy at every
observed system-call number, or the run is refused. During the run, a declared
deny returns its declared result through Detcore without an extra ptrace stop,
in the same way inside and outside a container.

**Limits, stated plainly:**

1. **Host-blocked calls are invisible to Detcore.** Inside a container, a call
   the host blocks never reaches Hermit: no syscall count, no virtual-time
   charge, no DETLOG record, no happens-before anchor, no replay record.
   Section 3 makes declared denies equally invisible outside the container, so
   for every call covered by a declared rule, accounting is the same in both
   places. Accounting differs only for a call the host blocks that the declared
   policy does not cover: that is, drift (limit 2) or an argument-conditional
   host rule nobody declared (limit 3). There, the guest silently gets the
   host's result inside the container and the real call outside, and INFO logs,
   syscall counts and virtual time diverge between the two. On one host, runs
   remain deterministic, because the host filter is a fixed function of the
   call.
2. **Drift is detected only at the sampled points.** The startup probe compares
   the host's actions at the probed numbers and argument vectors. A host change
   that affects only other arguments, unprobed numbers or a different
   instruction pointer goes unnoticed until someone runs with
   `--seccomp-paranoid`.
3. **Argument-conditional host rules stay invisible** unless the operator
   declares them. The probe sees them only at its own arguments.
4. **A supervisor-backed (USER_NOTIF) host filter** may act on calls the probe
   issues before the probe can tell. This is bounded but not removed
   (section 4).

`--seccomp-paranoid` (section 9) removes limits 1 to 3 for one run, at about
2.9× per traced call. It checks every call and stops the run on the first call
where the host disagrees with the declared policy.

## Design

### 1. Admission at startup

Admission runs once, in the parent, before stdin is read and before either
`--verify` child exists.

1. Read `Seccomp`, `Seccomp_filters` and `NoNewPrivs` from `/proc/self/status`.
   - **Mode 0:** nothing is inherited. A declared policy is enforced as in section 3.
   - **Mode 1 (strict):** refuse; Hermit cannot run.
   - **Mode 2:** continue.
2. **The run config does not acknowledge the filter** (it has no `seccomp:` section): apply the unacknowledged-filter policy below. The default refuses **without probing**.
3. **The run config acknowledges it:**
   - Unless the section says `probe: never`, run the startup probe (section 4, about 6 ms) and compare its canonical observation rows with the recorded ones.
   - A difference refuses, listing each changed number with its old and new action.
   - Then apply the **equality check**: at every observed number, the declared policy's action at the probed arguments must **equal** the observed host action (the same action, errno and TRAP data). It must not be merely stronger. Under normal transport, Detcore never sees a call the host blocks, so it could not enforce a stronger declared rule there; refusing at admission keeps the run config truthful.

**The unacknowledged-filter policy is a single switch.** The case "an inherited filter is present and the run config does not acknowledge it" is decided by one enumeration with two values, in exactly one function that every caller consults:

- `Error` (the default): refuse before the guest starts, as a policy refusal (exit 122), with the remedies in section 7.
- `Warning`: print the same text as a warning and continue. The run is then not reproducible from its config, and every record the run writes (its run evidence, any recording, any verification report) carries the same warning.

Changing the default is a one-line change. No other case has a switch: changed observations and a failed equality check always refuse.

### 2. The `seccomp:` section of the run config

It is OCI/Docker-profile-shaped and typed. The `inherited` block is evidence about the host, never itself a policy:

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
    observations:                   # canonical rows; every other probed number was observed allowed
      - {nr: 63, name: uname, action: errno, errno: 13}
      - {nr: 110, name: getppid, action: errno, errno: 1}
    observations_sha256: <hex>      # over the canonical rows; a shortcut only
    filters: 1                      # Seccomp_filters at capture; informational
```

- **Rule actions:**
  - ALLOW, LOG (the same as allow for the guest) and ERRNO in phase 2.
  - TRAP (with its data), KILL_THREAD and KILL_PROCESS later (section 3). Until then, a declared rule with one of these actions, or an observed host rule that would require one, is refused with a message naming the rule.
- **Default actions:** ALLOW and ERRNO.
- **Argument operators:** OCI's `NE`, `LT`, `LE`, `EQ`, `GE`, `GT` and `MASKED_EQ`, applied to argument registers (as seccomp does), never to memory.
- **Refused, each with a message:** `SCMP_ACT_NOTIFY`, `SCMP_ACT_TRACE`, `listenerPath`, filter `flags`, and unknown keys.
- **Typed load and save.** The run config's loader turns option keys into command-line tokens, and a typed section cannot travel that way. The loader therefore returns the parsed section beside the rewritten argv, `RunOpts` carries it, and `--save-config` writes it back unchanged. A load-save-load test pins this.
- **Recapture** replaces the `inherited` block and the `origin: observed` rules, and keeps every `origin: declared` and `origin: profile` rule.
- **`--passthru-opt`** (a partial syscall subscription) is refused together with a `seccomp` section, because unsubscribed calls bypass Detcore and the declared denies.

### 3. Enforcement: a Detcore deny check that accounting never sees

The declared rules compile into a lookup that Detcore consults first in its
syscall event handler. That means before the signal-phase check, before the
scheduler check-in (`pre_handler_hook`), and before the DETLOG record, the
syscall count and the virtual-time charge. A match skips the call and returns
the declared errno, then returns from the handler. It commits no scheduler
turn, charges no time, counts nothing and writes no INFO record; it is logged
at DEBUG, outside the compared INFO stream. Every backend that hosts Detcore
gets the same check, and none of them needs an extra stop:
- under ptrace, the check runs at the TRACE stop that every call already has in a fail-closed run;
- in-guest backends run it at their existing dispatch.

**Why the deny is invisible to accounting.** Inside a container, a call the
host blocks never reaches Detcore. The equality check (section 1) makes every
observed host-blocked call a declared deny. If Detcore accounted declared
denies outside the container, the same run would count, charge and log a call
there that it never sees inside. Making the deny invisible in both places is
what keeps accounting identical for every declared call (limit 1).

**Alternative considered: Hermit's own seccomp ERRNO rules.** The ptrace
backend could add ERRNO rules for the declared denies to the filter it already
installs. The kernel would then return the errno with no stop at all, and the
calls would be invisible to Detcore by construction. That needs a change to
Reverie's filter construction (the filter is built from the Tool's syscall
subscription in reverie-ptrace), which is a Reverie interception-model change
under the project's review rules. It would also cover only seccomp-based
backends. The Detcore check is a Hermit-only change, applies to every backend,
and is exact for ERRNO. The BPF route stays the candidate for TRAP and KILL,
whose kernel semantics (the real SIGSYS `siginfo`, the kill scope) a filter
reproduces exactly and an emulation would have to match field by field.

### 4. Observing the host: the safe probe

The probe runs only with consent: through the explicit capture command
(section 5), or as the startup drift check of a run config that already records
observations. It probes no number other than the canaries until every condition
below holds. A failed condition refuses (exit 122) and names the condition.

1. **A sacrificial child.** It is single-threaded, in its own session and process group (so `kill(0, …)` reaches only itself), with every file descriptor closed. It also unshares user, PID, network and mount namespaces where the host allows that; this is recorded either way.
2. **Non-dumpable, checked positively.** After `PR_SET_DUMPABLE 0`, `/proc/<child>/status` must be owned by root, which the kernel does only for a non-dumpable task. That was measured: uid 0, versus the tracer's own uid for a dumpable control child. Without this, every KILL rule invokes the host's core handler (about 400 ms each when cores are piped to a user-space handler).
3. **Filter installation checked positively.** `Seccomp_filters` in `/proc/<child>/status` must go up by exactly one after the child installs Hermit's filter. A return code is not evidence, because an inherited `ERRNO(0)` can fake it.
4. **Skipping checked on canaries.** The tracer skips `getpid`, then `gettid`, at their TRACE stops (`orig_rax = -1`). Each exit stop must report `-ENOSYS` (measured: `-38`), not an ID. That shows the tracer's register write took effect, even if an `ERRNO(0)` aimed at `ptrace` returned success.
5. **No fake success and no supervisor.** A probed number that returns without a TRACE stop and with a non-negative result aborts the probe. The host is refused as unsupported ("a filter fakes success or a supervisor answers calls"). The canaries are probed first.
6. **Signals checked.** A SIGSYS stop counts as a TRAP only with `si_code == SYS_SECCOMP`, `si_syscall` equal to the probed number and `si_arch` equal to the probed architecture. Its `si_errno` is recorded as the TRAP's data. Any other signal aborts the probe.
7. **Kill scope resolved.** A number that kills the single-threaded child is probed again from the second thread of a two-thread child, which distinguishes KILL_THREAD from KILL_PROCESS. If neither outcome is clean, the number is left out and named, for the operator to declare.
8. **Bounded.** Every probed number has a deadline (proposed: 100 ms), and the whole probe has one too. On expiry the child is killed and the probe refuses ("a probed call blocked; a supervisor may be holding it").

**Residual risk.** A USER_NOTIF supervisor that answers `CONTINUE` for a number
the probe reaches before step 5 catches it runs that call in the sacrificial
child, with zero arguments. Conditions 1 and 8 bound the damage; nothing removes
it. That is why probing needs consent and why `probe: never` exists.

**Evidence is sampled.** Two filters that differ only on a non-zero `ioctl`
request produce identical observations. Observations never establish that the
declared policy matches the host everywhere (limits 2 and 3). Authoritative
evidence about the whole filter comes only from an operator-supplied profile or
a host-side reader (section 5), and exact per-call evidence only from
`--seccomp-paranoid`.

### 5. Capture and profiles

- `hermit run --capture-seccomp --save-config FILE [OPTIONS] [-- PROGRAM ARGS]` runs admission step 1 and the safe probe. It then writes the run config with the `seccomp` section (merged as in section 2), prints the observed rows, and **exits 0 without running the guest**. This is the bootstrap that the default refusal points to.
- `--seccomp-profile PROFILE.json` imports an OCI profile, for example the container runtime's own default profile, as the declared policy, with `evidence: profile`. If observations exist, the equality check compares them with the profile at the observed numbers.
- Later, optionally: `hermit seccomp capture --pid PID`, run on the host outside the container. Where the kernel allows it (`CONFIG_CHECKPOINT_RESTORE`, `CAP_SYS_ADMIN`, and the caller itself unfiltered), it reads the real BPF and emits exact argument rules.

### 6. Guest-visible seccomp metadata

Detcore's procfs sanitizer passes `Seccomp_filters` through today, so the
physical filter stack would show, and a run inside a container would differ
from the same run outside it. Detcore instead presents the fields from the
explicit policy:
- `Seccomp: 2`.
- `Seccomp_filters`: what a guest sees today with Hermit's filter alone, plus one when the run config declares a `seccomp` section.
- `NoNewPrivs`: the value Reverie establishes.

### 7. Refusal messages

Each refusal names what failed and what to run next.

- **An inherited filter that is not acknowledged** has three remedies:
  - capture it: `hermit run --capture-seccomp --save-config run.yaml …`, then `hermit run --config run.yaml`;
  - import the runtime's profile with `--seccomp-profile PROFILE.json`;
  - or run in a container created without a filter (for Docker, `--security-opt seccomp=unconfined`). A filter cannot be removed from inside a running container, so the container has to be recreated.
- **Ptrace denied:** said separately. Granting ptrace (for example `--cap-add=SYS_PTRACE`) neither removes the filter nor acknowledges it.
- **A failed equality check:** name the number, the declared action and the observed host action.
- **A refused capture:** name the probe-safety condition that failed.

### 8. Record and replay, and runs that execute the guest more than once

- **Recording metadata**, which has no policy field today, gains the resolved policy and the evidence block.
- **`hermit record` and `hermit replay`** go through the same admission as `hermit run`. Replay enforces the recorded declared policy for the calls it executes; replayed results come from the log.
- **A replay host with inherited filters** is compared with the recording's evidence like any run config. If it differs, or the recording has none, replay refuses with the capture remedy.
- **Recordings made before this change** replay as today on mode-0 hosts, and are refused on mode-2 hosts unless the replay command line supplies a run config that acknowledges the filter.
- **`run --verify`** (a testing feature that runs the guest twice): admission runs once in the parent, and both children receive the same resolved policy, exactly as an ordinary run would.

### 9. Paranoid mode: per-call checking (`--seccomp-paranoid`)

This is revision 2's entry-stop interception, demoted to an opt-in mode for
runs where reproduction across hosts is in question. Reverie resumes guest
threads with `PTRACE_SYSCALL`. The syscall-entry stop arrives before any
seccomp filter runs, so for each call:

- If Hermit's TRACE stop arrives, the host allowed the call.
- If the exit stop arrives with no TRACE stop, outside the expected-no-TRACE contexts below, the host blocked it.
- A host-blocked call that the declared policy allows stops the run with a typed refusal naming the call, its arguments and the host's result.

Cost: two extra stops per call, measured at about 2.9× per traced call.

**Paranoid mode only observes; it does not change guest-visible behavior.**
Declared denies still go through the Detcore check (section 3). Detcore still
decides each call once, at the same logical event as in normal transport. The
extra entry and exit stops are transport only: they create no syscall event, no
scheduler turn, no time charge and no replay record. A paranoid run of a
compliant guest therefore produces the same outputs, INFO log and replay events
as a normal run.

**Transport state machine (per thread).**

| context | stops seen | TRACE expected | Detcore decision |
| --- | --- | --- | --- |
| ordinary guest call | entry, TRACE, exit | yes | at TRACE, as in normal transport |
| declared deny | entry, TRACE, exit | yes | at TRACE; skipped and invisible (section 3) |
| `rt_sigreturn` (Hermit's filter allows it) | entry, exit | no | none, as today |
| a call Reverie injects (from its private instruction window, which Hermit's filter allows) | entry, exit | no | none; the tracer knows it injected the call |
| a kernel passthrough number (uretprobe, uprobe) | entry, exit or SIGILL | no | none, as today; counted in a transport counter |
| a call interrupted by a signal | entry, [TRACE], exit with `-ERESTART*`, signal stop, then a new entry for the restart | per the restarted call | once, for the original call |
| `execve` | entry, TRACE, `PTRACE_EVENT_EXEC`, exit | yes | at TRACE |
| the thread dies inside the call | entry, [TRACE], `PTRACE_EVENT_EXIT` | n/a | at TRACE, if it arrived |
| `clone` | the parent's entry, TRACE and exit; the child's first stop is not a syscall stop | yes, for the parent | at the parent's TRACE |

The kernel passthrough list is detected per kernel and architecture: such a
number gets no TRACE stop even on a mode-0 host. It is recorded in the evidence
and counted visibly, never reported as host interference.

## Acceptance matrix

Each phase lands with the rows that apply to it.

- **Probe safety:**
  - USER_NOTIF answering `CONTINUE`, emulated success and emulated error, `ADDFD`, a missing listener and a hung one;
  - `ERRNO(0)` on filter installation and on `ptrace`;
  - a failed non-dumpable setup.

  Assert that no probed call executes, that no core handler runs, and that cleanup completes within the deadline.
- **Signals and termination**, for the later TRAP and KILL phase:
  - an asynchronous SIGSYS versus a seccomp SIGSYS;
  - blocked, ignored and caught dispositions;
  - TRAP payload and registers;
  - KILL_THREAD with surviving siblings;
  - KILL_PROCESS with blocked siblings;
  - robust-futex and clear-TID cleanup.
- **Observations:**
  - recorded versus changed rows, including changed errno, TRAP data and kill scope;
  - an extra ALLOW filter stacked on top (observations unchanged; guest-visible metadata unchanged);
  - the equality check refusing a stronger declared rule.
- **Invisible declared denies:** the same guest and the same declared ERRNO policy, run on a host whose filter blocks those calls and on a host with no filter. Outputs, syscall counts, continuous virtual time, happens-before anchors, INFO logs, procfs seccomp fields and replay events must all be equal.
- **The stated limit, demonstrated:** a host rule on a non-zero argument that nothing declared. A normal run on that host and one off it differ, and `--seccomp-paranoid` on that host refuses on that call.
- **Paranoid transport:** injection, skipping, restarts, `clone`, non-leader `exec`, fatal signals, `rt_sigreturn` and kernel passthrough numbers. Paranoid and normal runs of a compliant guest produce equal observations, and transport counters are checked separately from guest accounting.
- **Config and replay:**
  - load-save-load preservation of the typed section;
  - command-line precedence;
  - recapture that keeps declared rules;
  - legacy recordings;
  - replay hosts with no, matching and changed filters.
- **Refusal UX:**
  - run the printed remedy in a default Docker container;
  - refuse before any probe, guest launch or stdin read;
  - keep unsupported capture, a missing acknowledgement, denied capabilities and a failed equality check distinct.

## Phases (each lands separately)

1. **Admission:** the `/proc` check, refusal before any probing, the policy switch, refusal messages and guest-visible seccomp metadata. No probe and no enforcement yet: mode-2 hosts refuse under `Error`.
2. **The typed `seccomp` section and the invisible Detcore deny check for ERRNO rules:** recording metadata, and `--passthru-opt` refused with a section.
3. **The safe probe, capture and the equality check:** `--capture-seccomp`, observation rows, drift diagnostics and the kill-scope probe.
4. **`--seccomp-paranoid`:** the transport state machine.
5. **TRAP and KILL rules:** through Hermit's own filter (Reverie change) or an exact Detcore emulation, decided then. Also `--seccomp-profile` import, and optionally the host-side BPF reader.

## Open questions

1. **Detecting a USER_NOTIF host before probing.** No unprivileged signal is known to expose it. The answer here is layered: probing only with consent, the canaries and the fake-success abort, and deadlines. Should a mode-2 host without an imported profile refuse capture altogether instead?
2. **Probe argument vectors.** A second vector (for example all bits set) costs about 6 ms and narrows limits 2 and 3 without closing them.
3. **Run config schema version.** `hermit-run-config/v2` when `seccomp:` is accepted, or stay at v1, because the key only moves from refused to accepted. A v1 reader refuses the key, so either way an older Hermit fails safe on a newer file.

## Review history

**Revision 1** (a4327d92) received "changes requested". **Revision 2**
(105cb732) answered every finding with per-call checking on every filtered
host. The owner rejected that cost, so revision 3 answers the findings this way:

| finding | resolution in revision 3 |
| --- | --- |
| 1. The probe can execute the probed call | No probing without consent; refusal comes before probing. Positive checks, the fake-success abort, signal validation and deadlines (section 4). The residual risk is stated. |
| 2. A stronger declared rule cannot be enforced on host-hidden calls; accounting differs | The equality check refuses stronger declared rules (section 1). Declared denies are invisible to accounting, so declared calls account identically inside and outside a container (section 3). The remaining difference, on undeclared host blocks, is limit 1. Exact checking is available as `--seccomp-paranoid`. |
| 3. Observations prove sampled equality, not dominance | Observations are labelled sampled. Drift off the samples is limit 2. Authoritative evidence comes only from profiles; exact evidence only from paranoid mode. |
| 4. Kill scope, TRAP payload, asynchronous SIGSYS | The probe resolves kill scope and validates SIGSYS (section 4). TRAP and KILL enforcement is deferred to phase 5; until then such rules are refused. |
| 5. `Seccomp_filters` is guest-visible | Presented from the explicit policy (section 6). |
| 6. Paranoid mode needs a full tracing state machine | Section 9, with exactly one logical event per call. |
| 7. Record and replay are unspecified | Section 8. |
| 8. Capture cannot get past the refusal; typed config transport | `--capture-seccomp` exits without running the guest. Typed section carried beside the argv (sections 2 and 5). |
| 9. A hash alone cannot give drift diagnostics | Canonical observation rows (section 2). |
| 10. Docker remediation | Section 7. |

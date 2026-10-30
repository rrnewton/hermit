# Chosen process IDs and reserved input

Hermit's Chosen mode gives a `--no-namespace` guest reproducible process and
thread IDs, on the ptrace path with sequentialized threads. A required base
(`--pid-base`, strict selection, or a compatible replay) refuses a creation
that cannot retain those IDs. The existing tolerant route instead issues the
original native call and reports its host ID honestly. A fallback is not a
successful Chosen creation. Default runs in Hermit's PID namespace do not
request the facility described here.

Chosen clone rewrites use one backend-owned, read-only private page at
`0x71010000`, separate from the injection trampoline and signal-repair page.
The native base page is 4, 16 or 64 KiB on the admitted native ABI. Linux must
provide `MAP_FIXED_NOREPLACE` and `mseal` (Linux 6.10 or newer), and permit the
setup syscalls. An occupied address, unsupported facility or unproved setup
is named unavailability. Other backend implementations currently provide no
such facility. There is no fallback to application stack storage.

This page is observable. It appears in maps, can be read, occupies an address
that a native program could otherwise map, and cannot be overwritten,
unmapped or made writable by ordinary guest operations. Native mapping hints
and address-space probes can therefore differ. It is not claimed to lie
outside every kernel allocation search range. Chosen argument/output aliases
into it are refused before the rewrite; a tagged or overflowing pointer whose
effective range cannot be proved disjoint is also unavailable.

## Schedule JSON versus binary record/replay

New qualified Chosen schedule JSON carries this optional full-record member:

```json
"reserved_input": {
  "contract": "sealed-private-input-v1",
  "base": "0x71010000",
  "page_bytes": 4096,
  "abi": "x86_64"
}
```

The page size and ABI describe the actual authenticated target-image setup.
The member is in the existing `preemptions` object for a slice-boundary
recording. It is separate from the timing format: format 3, recorded time
jumps, their indices, reasons and targets retain their existing meaning.
Only an actual qualified producer supplies this declaration. Requesting
Chosen in Config, constructing a record, editing a file, or flushing partial
output does not qualify it. An unqualified executed image permanently removes
the producer's qualification; a later exec cannot repair that history.

**All pre-facility Chosen JSON without this provenance is refused before
guest startup.** Missing, unknown or malformed provenance, or a different
ABI/address/page extent, produces
`HERMIT_CHOSEN_JSON_INPUT_PROVENANCE`. No override can make an old file
compatible. Record the run again with the current Hermit, or use its producing
Hermit to replay the original. This is an explicit compatibility restriction,
not a successful replay of the old requirement. JSON remains editable; the
declaration is not a cryptographic attestation. Older binaries may ignore it
and are not certified to replay these new records.

Every schedule and priority loader checks the decoded input it consumes,
including raw Config with both paths. Analyze, bisect, minimize, prefixes and
identity interpolation carry source provenance. Combining Chosen sources
requires matching recognized declarations even when the result is an unchanged
endpoint. An editor cannot upgrade a legacy file by reconstructing it.
Non-Chosen JSON retains its existing admission; unknown optional metadata is
preserved without interpretation.

The **binary** record/replay stream has no reserved-input provenance. Raw
Config Chosen binary recording and binary replay therefore both refuse before
startup with `HERMIT_CHOSEN_BINARY_INPUT_PROVENANCE`, including a new recording.
The check uses the actual Recorder/Replayer type as well as the Config mode
flags, so false or mismatched flags cannot bypass it. Custom Tool compositions
must declare their real stream role. Use Namespace IDs for binary
record/replay, or plain Chosen with qualified schedule JSON. A JSON side file
and the current binary record epoch do not certify a binary Chosen stream.
Default Namespace binary record/replay and ordinary plain execution retain
their existing admission.

## When later Chosen rewrites become unavailable

The page must be free of covered concurrent readers and forced writers during
initialization and native input consumption. Ordinary guest syscalls keep
their native semantics; the following facts restrict later Chosen rewrites,
through the existing strict refusal or disclosed tolerant host-ID route.

| Cause | Scope and consequence |
| --- | --- |
| An outstanding background operation with an overlapping or unknown input span | Local to a proved address space, otherwise global, until native completion **and** deterministic readmission. The rewrite does not wait behind a vfork barrier for the operation to settle. |
| Native rseq registration without a transitive lifetime proof | Monotone local unavailability for that address space. The closure includes the registered area, mutable `rseq_cs`, descriptor and abort signature; a currently disjoint area is insufficient. Native-rseq opt-out and strict Chosen are incompatible for glibc 2.35+ processes that register rseq. Default refused-rseq behavior is unchanged. Copied-child inheritance and actual successful exec reset are checked; raw no-exec launches must admit inherited registration too. |
| Persistent reserved-range references or force access | Named exposure for userfaultfd, vmsplice/zerocopy, memory merging, process-memory write/export, direct unknown-ioctl argument overlap, robust-list/clear-TID overlap and unproved known persistent imports. Disarm, failed registration or a descriptor close does not automatically recover proof. |
| Mutable ancillary export | A potentially external `sendmsg` or `sendmmsg` with no immutable native header/control import retains process-memory and shared-description export uncertainty for the run, even if an earlier readable snapshot listed no descriptors. The current injection preparation does not certify an immutable native lookup, even for a sampled guest-held socket under a serial grant, so that case retains the same cost. A negative descriptor or zero `sendmmsg` count is a no-import control. The send itself is unchanged. |
| Kernel-armed SIGKILL | Parent-death SIGKILL, descriptor SIGKILL notification and POSIX timer SIGKILL lose proof before arming. New PID-namespace-init and effective child-exit SIGKILL causes activate at verified birth, so the creating NEWPID operation retains its positive contract. Once active these causes are monotone and run-wide, including across exec. Supervisors, zygotes and kill-on-notification workloads can lose later Chosen support. |
| Unclassified descriptor acquisition on a possibly shared native file table | A blocking `recvmsg` import or FIFO open can prevent Chosen run-wide until classification and readmission. A proved private table removes only another task's slot-use hazard, not prearmed notification or foreign effects. Fresh socket, socketpair, pipe, pipe2 and native accept results have their mandatory fresh-object classification. |
| Original clone3 whose lifecycle input is not immutable | `clone-input-unverified` is published before the unchanged native call. Proved no-child completion retires it; unproved birth/outcome retains monotone `clone-lifecycle-unverified`. It is not counted as a Chosen success and does not alter native error precedence. |
| Unproved terminal ownership or frozen unknown-target native access | Retained with its actual local/global scope. Ordinary singleton SIGPIPE and local-only group exits do not acquire a blanket global cause. The frequency of unknown-target cases in real workloads is unmeasured. |

A background read or write whose native descriptor lookup is unbound also holds a
transient unknown-target lease: a shared-table peer may replace the descriptor
before the kernel looks it up, and the ordinary injection prologue does not
currently bind even a private-table snapshot to that lookup. The lease ends
only after native completion, copyout and deterministic readmission. Immutable
negative descriptors, zero counts and native-invalid scalar positional offsets
are no-access controls. This is separate from a permanent force-write or
exported-authority cause; it does not replace either persistent obligation.

Initial-state admission also requires actual-object evidence for writable
`cgroup.kill`, preopened `/proc/sysrq-trigger`, native CPU/RTTIME limits and
inherited SIGKILL notification state. A guest pathname or modeled descriptor
alone is insufficient. Uninspectable physical state gives named unavailability;
Hermit does not change the limits, descriptor flags or arming syscall to make
admission succeed. Guest exports that grant untracked continuing authority
retain their named exposure.

A signal-handler Chosen clone nested while an outer input transaction is
still Preparing receives `input-transaction-busy`: strict/required Chosen
refuses and tolerant mode uses its honest native route. Nesting support is
not implied merely because the outer page has not yet been filled.

## Remaining boundaries

For an unknown ioctl, only its direct argument register is classified for a
persistent pointer. A driver can retain a nested pointer in a structure at an
ordinary address; that case is **uncovered**, as are other unclassified driver
effects such as deferred killing. Neither a read-only page nor sealing closes
that gap. The existing untraced trampoline escape
(https://github.com/rrnewton/reverie/issues/1064) is also uncertified. This work
does not repair old untraced-child accounting.

The environment must not permit an outside holder of a shared open-file
description to mutate its notification or other relevant state during protected
execution. Such a holder need not be root. Native before/after snapshots are
not an atomic proof against that mutation; guest-held/exported authorities are
accounted for separately.

External fatal termination, the launcher's parent-death SIGKILL when its Hermit
owner dies, and cgroup `memory.max`/OOM kills are bounded external failures,
including resource exhaustion under a guest-configured delegated limit. They
are not certified continued staged execution. An unresolved initialized page
must be normalized and verified in full before fallback or continuation;
failure to do so fails the session. No successful kernel output is restored
over, no replay comparison is relaxed, and virtual time is never rounded,
frozen or reset to accommodate the mechanism.

# Classifying a `--verify` divergence

Read this before triaging a divergence. There are **three** classes, they need
different fixes, and the obvious discriminator misclassifies one of them.

## The discriminator that works

**Look at what differs AT the divergent record**, not at the summary counts:

| what differs at the divergent record | class |
| --- | --- |
| a time field | pure-clock |
| control flow, visible as different DETLOG counts | true guest divergence |
| payload bytes, with everything else identical | pure-observation |

> [!WARNING]
> **Identical DETLOG counts does NOT mean pure-clock.** Pure-observation
> divergence has identical counts on both sides and is not clock-related at all.
> Classifying on counts alone misroutes it to a clock fix that cannot work,
> because the clocks already agree byte-for-byte.

Apply it directly. The verify report names the record, and the retained logs let
you see what actually differs there:

```bash
hermit run --strict --verify --verify-strict --verify-json report.json -- <prog>
# the report names the location; the run prints the two retained log paths
diff <(sed 's/^[0-9T:.-]*Z*//' PATH_PRINTED_FOR_RUN_1) \
     <(sed 's/^[0-9T:.-]*Z*//' PATH_PRINTED_FOR_RUN_2) > d.txt
grep -cE '^[<>]' d.txt                       # how many lines differ at all
grep -E '^[<>]' d.txt | grep -c '\[syscall\]'   # control flow?
grep -E '^[<>]' d.txt | grep -c 'iobuf'         # payload only?
```

A run whose differing lines are **all** `iobuf` and **zero** `[syscall]` is
pure-observation. That single ratio settles the class faster than reading either
log.

## The three classes

### 1. Pure-clock

Streams identical except for time fields.

### 2. True guest divergence

Different DETLOG counts **and** a real behavioural difference. Verified
instance, measured by `agent(hermit-006)`: counts 1030 versus 1032, with `wait4`
under `WNOHANG` returning 7 in one run and 0 in the other. The clock offset that also appears is *downstream* of
the behavioural split, not the cause — which is why counts, not clocks, are the
signal here.

### 3. Pure-observation

Identical control flow **and** identical stream shape — same DETLOG count, same
record index, same syscall, same return value, same buffer length, same exit
code — with the only difference being **the content of one buffer**.

The guest is deterministic. What it *read* was not.

Verified instance, `language-runtimes/python-dict-hash-iteration` and
`language-runtimes/python-io-subprocess-time`, both `verify/ptrace`:

```
compared_log_messages   left 35619 == right 35619      (and 35557 == 35557)
guest_exit_code 0, guest_signal null, both runs
whole-log diff          8 differing lines | 8 iobuf | 0 [syscall] | 0 other
```

Every differing line is a buffer hash. Not one syscall line differs — including
every `clock_gettime`, which returns `tv_nsec: 8311279…` identically in both
runs. The source is a host resource the guest can read:

```
#1069: socket(16, 524291, 0) = Ok(11)    <- AF_NETLINK, SOCK_RAW|CLOEXEC|NONBLOCK
#1073: sendto(11, …, 20, …)  = Ok(20)    <- netlink request
#1074: recvmsg(11, …)        = Ok(1468)  <- dump payload, hash differs
#1077: recvmsg(11, …)        = Ok(156)   <- hash differs
        561064f4ab1c29b1  vs  969b9620ad67e8af
```

That is glibc interface enumeration (`getifaddrs`/NSS), which CPython triggers
during startup. Confirmed natively by `agent(hermit-006)`: the differing bytes are interface
counters — twelve of twelve changed 64-bit fields increased and zero decreased,
which is what a monotonically-counting host interface table looks like.

Both cells share the fingerprint exactly: the same four buffers, at the same two
guest addresses, at the same two sizes. Two unrelated Python programs, one cause.

This class is the `/proc` family: **host state the guest can read**. It is fixed
by determinizing or excluding at the boundary the bytes enter through, not by
touching clocks and not by hunting guest behaviour.

## Not a class: a host input that changed during a run

A host file the guest opens can be replaced while a run is going; a package
manager rewriting `/etc/ld.so.cache` between the opens of two guest processes
is the measured case (`compat/sar-resource-tables`, 2026-10-05). Hermit numbers
inodes in the order it first sees them, so that run gets one inode more than the
other, and every later inode number differs by one. The first record that shows
it can be far from the file, such as the `d_ino` values of an unrelated directory
listing whose entries and order are otherwise identical.

`--verify` names this cause itself. Each run records the host identity (`dev`,
`ino`, size, mtime) of every file the guest opens, and which thread and syscall
opened it, in a side file outside the compared log that is written when the run
ends. A cause is named only when all of these hold:

- the two runs diverged, and the comparison was one strict enough for bitwise
  parity (`--verify-strict`), so everything before the divergence was compared;
- both side files are complete;
- the runs made the same opens, in the same order, up to an open where one run
  found a different host inode (`dev:ino`) at a path while the other found the
  one it had seen before: a replacement, the change that shifts Hermit's later
  inode numbers. A change of size or time alone is not named;
- in BOTH runs' logs, that open's finish record appears exactly once and before
  the first divergent record. A guest that replaces a file itself because it
  already behaves differently has diverged by then, so its divergence stays a
  failure. Positions are records in the log, so this holds where the log is
  not one stream in time order: SaBRe appends the records its guest forwards
  after the coordinator's own, each part is in time order, and the comparator
  reports the first difference in the earliest part.

Then the report's verdict is `infrastructure_error` with
`infrastructure_error.kind` `host_input_changed`, naming the run, the path and
both identities. The comparison and the divergence position are kept, stderr
carries a `HERMIT_HOST_INPUT_CHANGED` line, and the run still exits 1. The
validation harness reruns such a cell once, unless either run also breaks a
stdout assertion the cell declares; a second divergence stays a failure. The
row stays its own type, an infrastructure ERROR, and is never counted as a
pass.

Each open is reported to Detcore's global state with an observation request
that carries no logical time and is answered before any clock or scheduler
accounting; the global state keeps the run's records and writes them when the
run ends. Detcore sends the same request whether it runs in the tracer or
inside the guest, so capture reaches the ptrace, KVM and SaBRe backends.

A change is named as the cause only on a backend that observes every guest
process from its first instruction after exec: ptrace (with or without e9patch
preprocessing) and KVM. SaBRe's loader runs an exec'd program's
`.preinit_array` before Detcore starts and forwards those syscalls unobserved,
so a guest could have made the change in code Hermit never saw, and an
identical log prefix does not rule that out. There `--verify` prints
`HERMIT_HOST_INPUT_CHANGE_UNATTRIBUTED`, naming the change and the reason, and
the verdict stays a divergence. In-guest LiteInst records too, but refuses
`--verify` until it forwards its guest's records to Hermit. DBT has its own
launcher and verifier, which do not collect the records. On all three a
divergence stays red.

The one remaining case is a coincidence: a host file replaced during an attempt
before the divergence, and an unrelated flaky product divergence in that same
attempt. That attempt earns the one retry.

## Why this class is only now visible

`compare_io_buffers` became the default in
[#2384](https://github.com/rrnewton/hermit/pull/2384). Before that, buffer
**content** was never compared, so a byte-unstable read left the syscall stream
identical and the cell passed.

> [!IMPORTANT]
> **Every one of these cells previously reported bitwise parity while
> diverging.** The class is not new; the ability to see it is. A cell that turns
> red at the io-buffer boundary after #2384 is not a regression — it is a
> pre-existing divergence becoming visible, and the green before it was the
> defect.

Expect more of them: anything that resolves a hostname or enumerates interfaces
will read the same netlink reply. Two of two Python cells hit it; the rest of the
corpus has not been surveyed.

## Provenance

Established 2026-08-25 across three independent investigations, each of which
derived part of it separately. The class-2 instance and the native confirmation
of the class-3 bytes are `agent(hermit-006)`'s measurements; the whole-log diffs,
the netlink identification and the shared fingerprint across the two Python cells
are `agent(hermit-007)`'s. The two-class framing that preceded it, and its
"identical counts means pure-clock" discriminator, are superseded by this
document.

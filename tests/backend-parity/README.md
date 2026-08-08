# Hermit backend parity runner

This directory tracks executable parity contracts across Hermit's ptrace,
DynamoRIO (DBI), and KVM backends. The case catalog and its small set of known
gaps live in `run_matrix.py`; new cases are green contracts by default. Live
results are compatibility measurement state. Inside dev-hermit, the runner
writes one ignored artifact per invocation under
`compat-envelope/ignored/backend-parity/`. The tracked scorecard changes only
when a caller explicitly supplies `--parent-scorecard`.

## Current ratchet

The L1 ratchet (`--strict`, run three times) and the L2 ratchet (`--strict
--verify`, Hermit's backend-specific double-run verifier) are tracked
separately, because a contract can hold at L1 yet not at L2.

L1 (`hermit run --strict`):

| Backend | Passing contracts | Contract coverage |
| --- | ---: | ---: |
| ptrace | 28/28 | 100% |
| DBI | 26/28 | 93% |
| KVM | 27/28 | 96% |

L2 (`hermit run --strict --verify`):

| Backend | Verified contracts | L2 kind | Contract coverage |
| --- | ---: | --- | ---: |
| ptrace | 28/28 | stripped DETLOG | 100% |
| DBI | 25/28 | native self-verify | 89% |
| KVM | 26/28 | guest-visible only | 93% |

The L2 assurance kinds are not interchangeable. Ptrace's plain `--verify`
normalizes and compares DETLOG under the lossy `Stripped` policy; it is not a
bitwise claim. DBI uses a dedicated adapter that compares its two stdout/exit
results, native Detcore summaries, and guest-memory hashes, not DETLOG. KVM
compares guest stdout, stderr, and exit status while deliberately omitting its
nondeterministic internal trace order. These are within-backend consistency
witnesses, not cross-backend stdout parity.

The task's pre-existing DBI-native baseline is 70/89 tests (78.7%). That number
measures the backend's own Reverie suite. The 26/28 number above is deliberately
separate: it measures the cross-backend Hermit contracts in this directory.
The current DBI path satisfies the virtual clock, virtual PID, root-thread
random-source, process wait lifecycle, application executable-memory, and
file-mutation contracts, plus deterministic memory-advice and
memory-layout behavior. It is an explicit gap on the file-metadata row (see
below). It also deterministically refuses io_uring and listmount,
verifies that epoll remains available as a fallback, and refuses process-memory
reads and writes with deterministic `EPERM`. The wait contract covers deterministic
`wait4`/`waitid` results, at least one SIGCHLD handler delivery (standard signals
may coalesce), complete reaping, and zeroed child CPU accounting. The
executable-memory contract writes machine code into an anonymous mapping,
transitions it from writable to executable, and calls it.
The memory-advice row checks accepted and rejected advice, address validation,
and file-backed `MADV_DONTNEED` restoration; KVM instead enforces its documented
deterministic `ENOSYS` refusal for `MADV_DONTNEED`. The memory-layout rows check
that `sbrk`/`brk` growth, ordered one-, two-, and three-page private anonymous
mappings, and a written two-page shared anonymous mapping produce the same
address sequences across repeated runs of each backend; they deliberately
permit different backend-local layouts. Portable pthread startup still exits
or stalls intermittently during DynamoRIO startup, so it remains an explicit
gap rather than making the strict CI gate flaky. The random-source row continues
to use root-only mode so it measures the cross-backend root stream independently
of the pthread lifecycle gap.

The file-mutation row creates, writes, attempts allocation, truncates, renames,
links, reads, and removes temporary files without exposing backend-specific metadata.
The file-metadata row checks positional I/O, ownership and access operations,
hard and symbolic links, path/fd/symlink extended attributes, a shared file
mapping, readahead, and range synchronization. It permits documented filesystem
policy failures for extended attributes but not an unimplemented syscall. DBI is
an explicit gap on this row: it forwards `fchown` to the real kernel, so once
credential queries are determinized to virtual-root identity `0` (PR #1549) the
guest's `fchown(fd, 0, 0)` becomes an unprivileged chown-to-root and returns
`EPERM`, while ptrace remaps it through the user namespace. `fchown` is not
correctly implemented under DBI, and asserting against a half-implemented syscall
could pass by accident and prove nothing, so the DBI cell is a declared gap until
DBI determinizes `fchown`.
The io_uring fallback row requires all three io_uring entry points to return
deterministic `ENOSYS`, then checks that `epoll_create1` still succeeds.
The listmount row requires deterministic `ENOSYS` even when the host kernel
recognizes the syscall and returns `EINVAL` for the same request.
The process-memory refusal rows supply valid local and remote iovecs for
self-targeted `process_vm_readv` and `process_vm_writev` calls. Both require
deterministic `EPERM` without copying the source byte, while the same calls
succeed outside Hermit.

KVM loads dynamic Linux ELF programs through `KvmGuest<Detcore>` and passes
twenty-seven contracts, including its bounded cooperative pthread lifecycle,
executable memory, deterministic memory-advice policy, clock, PID, inert
scheduler-policy queries, synthetic CPUID, and
threaded random-source probes, plus file mutation, listmount refusal,
process-memory read/write refusal, io_uring refusal with epoll fallback,
repeatable heap growth, and private/shared anonymous mapping layouts. KVM
thread syscalls bypass per-child Detcore callbacks, but the shared personality
still provides distinct worker samples and byte-identical output across strict
verification runs. Its no-xattr filesystem model validates xattr targets and
arguments before returning deterministic Linux-compatible errors, while its
in-memory mapping model validates `msync` and translates range-advice file
descriptors. Serialized child exits support both `wait4` and `waitid`, including
canonical zero CPU accounting and complete reaping. The remaining process-wait
lifecycle gap is guest SIGCHLD handler delivery: the KVM personality records the
exit but does not yet synthesize an x86-64 signal frame to run the handler.

## Cases

Each cell shows the L1 status and, after `/`, the L2 status: `stripped` for
ptrace's normalized DETLOG comparison, `self` for DBI's native verifier,
`guest` for KVM guest-visible verification, and `gap` where the level is not
reached.

| Test | ptrace | DBI | KVM |
| --- | --- | --- | --- |
| `hello_stdout` | pass / stripped | pass / self | pass / guest |
| `argument_forwarding` | pass / stripped | pass / self | pass / guest |
| `exit_zero` | pass / stripped | pass / self | pass / guest |
| `exit_status` | pass / stripped | pass / **gap** | pass / guest |
| `file_read` | pass / stripped | pass / self | pass / guest |
| `file_mutation` | pass / stripped | pass / self | pass / guest |
| `file_metadata` | pass / stripped | gap / gap | pass / guest |
| `io_uring_fallback` | pass / stripped | pass / self | pass / guest |
| `listmount_unavailable` | pass / stripped | pass / self | pass / guest |
| `process_vm_readv_refusal` | pass / stripped | pass / self | pass / guest |
| `process_vm_writev_refusal` | pass / stripped | pass / self | pass / guest |
| `executable_mmap` | pass / stripped | pass / self | pass / guest |
| `memory_advice` | pass / stripped | pass / self | pass / guest |
| `heap_growth` | pass / stripped | pass / self | pass / guest |
| `anonymous_mmap_layout` | pass / stripped | pass / self | pass / guest |
| `shared_anonymous_mmap` | pass / stripped | pass / self | pass / guest |
| `pthread_lifecycle` | pass / stripped | gap / gap | pass / guest |
| `process_wait_accounting` | pass / stripped | pass / self | pass / **gap** |
| `process_wait_lifecycle` | pass / stripped | pass / self | gap / gap |
| `cpuid_policy` | pass / stripped | pass / self | pass / guest |
| `virtual_clock` | pass / stripped | pass / self | pass / guest |
| `random_sources` | pass / stripped | pass / self | pass / guest |
| `virtual_pid` | pass / stripped | pass / self | pass / guest |
| `scheduler_policy_queries` | pass / stripped | pass / self | pass / guest |
| `signal_disposition` | pass / stripped | pass / self | pass / guest |
| `sigaction_state` | pass / stripped | pass / self | pass / guest |
| `sigprocmask_state` | pass / stripped | pass / self | pass / guest |
| `sigaltstack_state` | pass / stripped | pass / self | pass / guest |

The `scheduler_policy_queries` contract pins Detcore's inert-scheduler-policy
model: the guest arms and re-reads an `ITIMER_REAL` one-shot against virtual
time, queries `ioprio_get` (fixed virtual default 0), and issues a
`sched_setattr` requesting `SCHED_DEADLINE`. That last call returns `EPERM`
outside Hermit (real-time scheduling needs privilege), but Detcore accepts it
as a deterministic no-op because it replaces the Linux scheduler with its own,
so the guest observes an identical, host-independent result across ptrace, DBI,
and KVM and across the `--verify` double run.

The authoritative exceptions and their reasons live in `L1_GAPS` and
`L2_GAPS` in the runner. The runner executes each passing pair three times and
checks exit status, stdout, and (for determinism cases) byte-identical repeated output.
Passing `--strict` adds `hermit run --strict` to every probe; the hosted DBI
gate uses this mode.
The DBI random-source contract also compares the root thread's post-fault
random stream byte-for-byte with a ptrace reference run. It deliberately uses
the fixture's root-only mode to keep that comparison independent of the
pthread lifecycle row.
Without `--strict`, repeat-run results are compatibility evidence rather than
an assurance level. With `--strict`, they are L1 strict-mode evidence backed by
three byte-identical runs. The runner disables PMU timeslicing for portability.

### L2 verification (`--verify`)

Passing `--verify` lifts every probe to L2: the runner invokes
`hermit run --strict --verify --verify-allow both`, so hermit itself runs each
guest twice and applies the selected backend's self-consistency check. Ptrace
compares exact stdout/stderr/exit plus a normalized DETLOG; DBI's separate
adapter compares stdout/exit, native Detcore summaries, and guest-memory hashes;
KVM compares exact stdout/stderr/exit without an internal trace. The runner
records those as within-backend evidence. It does not reinterpret an overall
verify PASS as cross-backend stdout parity.

Two contracts hold at L1 but not L2 and are recorded as L2 `gap`s with their
reasons in the runner:

- **`exit_status` on DBI.** The fixed nonzero-exit oracle passes three L1 runs,
  but `--verify-allow both` completes after the first DBI run exits nonzero, so
  no second run or self-consistency witness exists. The L2 row remains a gap
  rather than treating one successful expected exit as a double-run compare.

- **`process_wait_accounting` on KVM.** The `--verify` concurrent double-run
  races child reaping: `waitid` on the already-reaped child returns `ECHILD`
  (`No child processes`), so the second run exits non-zero and verification
  fails. reverie-kvm synchronizes `wait4` child state but not `waitid`. This is
  reproducible across repeated runs; L1's stdout-only, three-run check does not
  surface it, which is precisely the value of the L2 lift.

Hermit's KVM root process enters the shared tool through
`run_static_elf_with_tool::<Detcore>`, but child process and thread syscalls
currently execute in the backend's deterministic `ElfExecutor` personality
without per-child Detcore tool callbacks. The CPUID row similarly validates
reverie-kvm's backend-local `KVM_SET_CPUID2` policy, not Detcore CPUID-event
parity.

## e9patch preprocessing corpus

e9patch is not a backend in this matrix. It is binary-rewriting *preprocessing*
for the ptrace backend: e9tool rewrites the guest ELF ahead of time to pre-trap
its `SYSCALL` sites, then Detcore runs the rewritten image under ptrace. e9tool
rewrites only the *main* executable, so the dynamically linked libc guests above
expose zero in-ELF `SYSCALL` sites (`candidate_sites=0`) and never exercise the
rewrite path — which is why e9patch is not a column here. Its parity is instead
ratcheted by `e9patch_corpus.py` over a freestanding, statically linked,
raw-`syscall` corpus under `e9patch_corpus/`, where `candidate_sites > 0`.

For each guest that harness enforces exit-status parity, stdout parity, golden
L2 (`hermit run --strict --verify`), e9patch L2
(`hermit --backend e9patch run --strict --verify`), full direct-AOT coverage
(`mapped_sites == candidate_sites > 0`), no signal fallback (`b0_sites == 0`),
and guest-syscall DETLOG **tail-match**: the golden guest-syscall sequence
equals the suffix of the e9patch sequence. Byte-identical DETLOG parity is
impossible by construction because the e9patch image runs a fixed deterministic
e9loader prologue (readlink/open/arch_prctl/`N`×mmap/close) before the guest's
`_start`; that prologue is a pure prefix, so the enforced parity is guest-syscall
DETLOG identity *modulo* the deterministic prologue, plus L2 and guest-visible
parity. No strict-detlog-identity claim is made.

Like the KVM `/dev/kvm` gate, this harness is `BLOCKED` in CI: it needs a hermit
built `--features e9patch` and a built e9tool/e9patch pair
(`HERMIT_E9TOOL`/`HERMIT_E9PATCH_BACKEND`). Run it locally:

```bash
cargo build -p hermit --features e9patch
HERMIT_E9TOOL=<path>/e9tool HERMIT_E9PATCH_BACKEND=<path>/e9patch \
    python3 tests/backend-parity/e9patch_corpus.py \
    --hermit target/debug/hermit --require-backend
```

Use `--check` to validate the corpus contract without prerequisites.

## Splitting asymmetric backlog PRs

PRs that predate the shared-manifest symmetry guard may combine useful code
with additions to this backend-private corpus. Do not hand-edit those patches.
Plan a lossless split first:

```bash
tests/backend-parity/split_asymmetric_pr.py --pr <number>
```

The dry run assigns every changed path and hunk to code or deferred tests,
replays both partitions, and requires their union to reproduce the source PR's
Git tree exactly. It fails instead of guessing on mixed inventory edits,
private-test deletions, unknown asymmetry shapes, or code replay conflicts.

Publishing is a separate explicit operation:

```bash
tests/backend-parity/split_asymmetric_pr.py --pr <number> --publish \
  --role-tag '[impl agent, MODEL]'
```

A mixed PR becomes a code-only draft against fresh `main` and a test-only draft
against the source PR's original base. The latter is labeled
`matrix-asymmetric-tests-deferred` and carries a required next-action checklist:
promote through the shared ptrace front door, minimize then promote, or reject
with evidence. The welded source closes only after both replacements exist. A
test-only source stays open as the labeled deferred PR; the tool does not create
an empty code PR.

The open PR count can rise after a mixed split. That is intentional: one
unlandable PR becomes landable code plus explicit, queryable test debt.

## Running

Validate the case catalog and known-gap invariants without backend prerequisites:

```bash
python3 tests/backend-parity/run_matrix.py --check
```

Build Hermit, then enforce the ptrace baseline:

```bash
cargo build -p hermit
python3 tests/backend-parity/run_matrix.py --backend ptrace
```

Run DBI with the pinned DynamoRIO runtime and client built by Cargo:

```bash
cargo build --release -p hermit
python3 tests/backend-parity/run_matrix.py \
    --hermit target/release/hermit --backend dbi --strict --require-backend
```

Run KVM on a host with read-write `/dev/kvm` access:

```bash
python3 tests/backend-parity/run_matrix.py --backend kvm --require-backend
```

Enforce the L2 ratchet on any backend by adding `--verify` (it implies
`--strict`); hermit's own double-run then asserts the recorded L2 kind per
contract:

```bash
python3 tests/backend-parity/run_matrix.py --backend ptrace --verify --require-backend
python3 tests/backend-parity/run_matrix.py --hermit target/release/hermit \
    --backend dbi --verify --require-backend
python3 tests/backend-parity/run_matrix.py --backend kvm --verify --require-backend
```

Use `--probe-gaps` to execute documented gaps and report `XPASS` candidates
(in `--verify` mode the probe reports which L2 kind a gap actually reached).
Every non-check run auto-discovers an outer dev-hermit checkout and writes one
ignored per-run CSV below `compat-envelope/ignored/backend-parity/`. The runner
prints one absolute, shell-quoted, schema-aware command for a deliberate
reviewed fold-in; it maps columns by name and refuses a duplicate run ID. Use
`--parent-scorecard PATH` to opt into appending that exact file (including the
tracked scorecard), `--no-parent-scorecard` to suppress the observation artifact,
or `--output /tmp/backend-parity.tsv` for the legacy standalone observation TSV.
`BLOCKED` means a required host capability or runtime artifact was absent; it
does not change the known-gap contract.

# Basic Sanity M1, Medium Sanity M2, and Fail-Closed M3

This is the implementation brief for one focused Hermit implementor/tester.
It is not a coordinator handoff.

## Owner direction for this work

Work directly in:

- `/home/newton/work/dev-hermit/hermit`
- `/home/newton/work/dev-hermit/reverie` only when Hermit's pinned Reverie
  implementation must change

Stay on `main`, commit directly to `main`, and move the product forward. Do not
allocate slots, build a TaskGraph process around the work, wait for PR machinery,
or add new ledgers, receipts, labels, tiers, reason classifiers, or review-family
bookkeeping. Do not resume the prior ORC/coordinator strategy of auditing the
audits or creating process infrastructure around the work.

Keep Hermit's local validation implementation passing, especially:

```bash
./scripts/validate.rs --self-test
./ci/compat-envelope/scorecard.rs check
./validate.sh
```

Use focused tests during iteration and run the expensive full validation only at
meaningful checkpoints. Keep generated output under `ignored/` or `target/`, not
in Git and not as the only copy under `/tmp`.

This owner-directed workflow supersedes the branch/PR/coordinator workflow text
elsewhere for this focused mission. Basic safety still applies: inspect before
editing, do not reset or clean away work you did not create, keep commits focused,
and do not weaken a product assertion to make a test pass.

## Checkout state when this brief was written

On 2026-08-15 the primary Hermit checkout was:

- branch: `main`
- HEAD: `bbafa443315c42d441bcb388f9608a27943e81fc`
- locally recorded `origin/main`: `302a1a9c0fde564db0292d1ea5cabc91343e79bc`
- relationship: two local commits ahead and four upstream commits behind

Do not discard the two local commits:

1. `0ac2216f54d9bd5ff5eff74b0ee747d8d85559a4` — **Use the machine for compatibility pressure runs**
   - changes `ci/compat-envelope/pressure-test.rs` and `ci/test_harness.sh`
   - connects explicit `--jobs` to manifest-guest capacity
   - shares one read-only manifest snapshot rather than invoking Cargo for each cell
   - parallelizes independent fixture preparation
   - checks requested width against declared memory limits
   - retains CPU/memory profile fields
2. `bbafa443315c42d441bcb388f9608a27943e81fc` — **Run pressure checks from the authorized checkout**
   - changes `ci/compat-envelope/pressure-test.rs`
   - runs clean batch/repeated pressure work from the exact clean checkout rather
     than a generated nested clone
   - checks that HEAD and tracked source stay unchanged before and after execution

These commits are useful but **not accepted evidence that high-width pressure
runs work**. Both high-width attempts failed before any cell ran because
BPFJailer denied Cargo writes in the boxed build. Preserve the commits, reconcile
them with current upstream `main`, then fix and test the mechanism rather than
assuming it is complete.

A broader recovered pre-restart working state also exists at
`508907ea21` on branch `recovery/pre-restart-m1-wip-2026-08-14`. It is a safety
snapshot containing older and probably superseded hunks; compare it only when a
specific missing change is suspected.

## The simple model to preserve

There are two different jobs:

1. **Validate the green compatibility envelope.** Ordinary validation runs the
   cells currently declared green. Every run must freshly execute them. A green
   cell turning red is a regression to fix, not permission to quietly shrink the
   envelope.
2. **Pressure-test outside or around the envelope.** Run red cells after engine
   improvements to find red-to-green movement, and repeat selected green cells to
   find flakes. A red attempt may fail, crash, time out, or OOM without preventing
   later independent cells from running.

The scorecard belongs in this repository at `SCORECARD.md`. It is a concise,
version-controlled table derived from the manifest. Do not rebuild a second
canonical scorecard in the parent `dev-hermit` repository.

Green/red is the compatibility decision:

- deterministic under the current milestone's comparator: green
- anything else: red
- a cell that cannot run: red

Execution diagnostics such as nonzero exit, crash/error, timeout, OOM, no result,
or malformed evidence are useful failure categories. They are not compatibility
tiers. Do not reintroduce the abandoned reason/tier classifier.

Keep these two measurements separate:

- **same-backend determinism:** run the same backend twice and compare it
- **cross-backend parity:** compare another backend with a fresh ptrace reference

The current scorecard measures the first and explicitly does not yet contain the
second. Do not call same-backend repeatability cross-backend parity.

## Milestone 1 — Basic Sanity

### What the owner meant

M1 establishes that the current, pre-tightening compatibility score is clean,
fresh, reproducible, and knowable.

At this milestone the metric is still the old `hermit run --verify` behavior.
That comparator strips information and is weak. State that limitation every time
the M1 score is reported. M1 is not strict INFO-log determinism.

The intended M1 behavior is:

- every `validate` run freshly executes every green cell
- every run checks the scorecard table committed with the Hermit commit
- the table reports totals and per-backend/per-mode counts
- `custom` commands are not multiplied into the uniform compatibility denominator
- verify is the first expansion priority, then replay, then chaos
- a selected green failure is repaired rather than silently relabelled red
- repeated runs look for flakes, but do not fetishize exactly 20 repetitions of
  every cell when enough useful evidence already exists
- full/red pressure runs are occasional discovery runs, especially after a
  Detcore engine improvement
- dangerous red cells are individually boxed and bounded so a hang, crash, or
  OOM does not stop the rest of the population

The owner originally asked for five separate solid validation runs before
calling Basic Sanity established. Five clean validations did occur on successive
evolving SHAs (`832935a7`, `d44173a4`, `f9b96007`, `2332f726`, and
`e2f9a36b`; an earlier `2dd34a54` run also passed). That satisfies the count of
clean development checkpoints, but it is not five repeated measurements of one
unchanged commit and it does not replace the incomplete pressure population.
Later, the owner explicitly said not to get hung up on exactly 20 stress
repetitions for every cell. Use judgment: get discriminating evidence quickly,
use the whole machine for embarrassingly parallel work, and record the exact
population actually run.

### Current M1 score

At the M1 baseline represented by `SCORECARD.md`:

| Measurement | Green | Total |
| --- | ---: | ---: |
| All comparable cells | 170 | 5,376 |
| Verify | 167 | 1,680 |
| Replay | 1 | 1,680 |
| Chaos | 2 | 1,680 |

Backend totals are:

| Backend | Green | Total |
| --- | ---: | ---: |
| ptrace | 150 | 1,008 |
| DBT | 9 | 1,008 |
| KVM | 0 | 1,008 |
| SaBRe | 9 | 1,008 |
| LiteInst | 2 | 1,008 |
| native naked control | 0 | 336 |

This exact table has 336 manifest tests. Its denominator is:

```text
N tests × (5 Hermit backends × 3 modes + 1 native naked control)
```

For this dated example, `336 × 16 = 5,376`. Do not scatter the literal test
count through documentation; derive it from the current manifest because test
337 will eventually exist.

Ordinary full validation selects 172 cells/entries: the 170 comparable green
cells plus two custom commands outside the denominator. A chaos entry may expand
into more than one seed/verification invocation.

These numbers use the legacy stripped, same-backend comparator. They do not
prove strict INFO-log determinism and they do not prove cross-backend parity.

### What M1 actually achieved

A prior verified task note reports that exact-SHA validation at
`1b98ccef5d5098bbc3e72fba689b1a026aaeb780` passed:

- 59 of 59 validation nodes
- 1,065 Rust tests
- all 170 comparable selected cells once
- the two selected custom commands

The durable log/receipt handle was not found in this primary checkout during
handoff review. Preserve the task note as the provenance for this claim rather
than treating source inspection as a fresh validation.

At handoff, current fetched `origin/main` (`302a1a9c...`) did not have a known
exact validation result. Do not carry the `1b98ccef...` result forward as though
it validated the current tip.

The large repeated-green run is at:

```text
ignored/compat-envelope/m1-green-20x-1b98ccef5d50/
```

It planned 170 cells × 20 repetitions = 3,400 attempts at scheduler width 4.
It was interrupted before a terminal top-level summary:

- 2,752 result rows, all reporting PASS
- 2,748 clean terminal harness statuses
- four PASS payloads with harness status 141; exclude these as interruption
  damage rather than counting them as either product passes or failures
- 137 cells completed 20 of 20
- `language-runtimes/python-io-subprocess-time × verify × ptrace` retained eight
  clean repetitions plus four interruption-damaged rows
- 32 cells never started
- no completed cell had mixed PASS/non-PASS outcomes
- no top-level `summary.json`
- no top-level `runner-outcomes.json`

This is useful partial stability evidence. It is not a completed 3,400-attempt
population, and the 32 late cells are a biased missing population rather than
evidence that they are stable.

The run also demonstrated an obvious throughput defect:

- configured jobs: 4
- hard manifest-guest cap: 4
- maximum overlap inferred from retained attempt intervals: 4 attempts
- retained attempt timestamps span about 6,077 seconds; 2,752 rows over that
  span is about 27.2 attempts/minute, but it is not terminal whole-run wall time
- the machine had roughly 284 CPUs available to the safe-ci slice

The width-four run used only a tiny fraction of the machine. Raising `--jobs`
alone was insufficient because the manifest-guest cap was a separate ceiling.
The two local commits above try to fix this, but the high-width follow-ups are
non-verdicts:

- `m1-green-5x-0ac2216f-j235`: zero cell attempts; release build hit BPFJailer
  `FILE_OPEN`
- `m1-green-5x-bbafa443-j234`: zero cell attempts; both builds hit BPFJailer
  `FILE_OPEN`

Plain low-width Cargo builds still worked. High width was never reached: the
denial occurred in boxed pressure-build setup before any cell ran. Do not claim
width caused it, and do not request a broad sandbox exemption before checking
whether the boxed setup or path choice is self-inflicted.

### Meaningful M1 fixes already on upstream history

Do not redo these from scratch:

- `1f4e430c...`: moved scorecard and fresh-result enforcement into Hermit
- `5cb63368...`: added usable exact-cell/random-sample pressure commands,
  failure categories, retained logs, one-input log normalization, and JSON
  divergence fields
- `87bb8c14...`: fixed a timeout that could be followed by exit 0 and displayed
  as PASS
- `2dd34a54...`: stopped calling same-backend repeatability cross-backend parity
- `f9b96007...`: made pressure budgets match the real number of attempts
- `2332f726...`: stopped seedless chaos from inventing attempts and replaced
  unstable live-host `findmnt` input with a fixture
- `a5a219c8...`: removed the structural filter that forced every chaos cell red
  and made selected chaos use same-seed verification
- `5d4a8ac2...`: retained the executed-test count for a test node that ran and
  failed instead of labelling it zero-executed
- `91533e73...`: added repeated-green execution through the typed safe-ci runner
- `5dde81d0...`: stopped record/replay from letting 65 Determinized syscalls
  bypass Detcore
- `def2399cc...`: isolated concurrent bound-port tests while preserving the
  dup-alias assertion
- `1b98ccef...`: kept pressure checkouts visible to Hermit guests

### Important M1 findings that remain

- `SCORECARD.md` currently has no merged per-red-cell observations even though
  the mechanism exists.
- Cross-backend parity is not represented in the scorecard.
- Replay raw-log retention remains unfinished.
- The single replay green compares one recording with its own replay. There is
  no tracked scorecard cell comparing multiple independent recordings of the
  same guest, so `replay = 1` is not evidence of cross-recording determinism.
- Signal-caused failures are still grouped under `crash-error` rather than
  separately identified.
- The original bound-port investigation produced assertion failure, timeout,
  and tracee-SIGSEGV populations on another host. `def2399cc...` deliberately
  did not claim that address isolation explained those other outcomes.
- The current pressure runner can execute an exact cell and a bounded seeded
  sample, but its high-width path still needs a small, fast scaling test before
  another large population.
- The two local pressure commits make `ci/compat-envelope/README.md` stale: it
  still describes a temporary clone and serialized fixture preparation. Update
  that documentation if the commits survive reconciliation.

Useful fast commands:

```bash
# One exact red cell with a tight bound.
./ci/compat-envelope/pressure-test.rs run \
  --test applications/example-timed-progress-bar \
  --mode verify --backend ptrace \
  --cell-timeout 60 \
  --results ignored/compat-envelope/one-cell

# Ten reproducibly selected red cells; custom and naked are omitted.
./ci/compat-envelope/pressure-test.rs run \
  --sample 10 --seed 42 --cell-timeout 60 \
  --results ignored/compat-envelope/sample-10

# Repeat one known cell without launching a full matrix.
./ci/compat-envelope/pressure-test.rs plan \
  --test backend-parity-c/fork-exec-pipeline \
  --mode verify --backend ptrace \
  --repetitions 100 --cell-timeout 120 --jobs 64 \
  --results ignored/compat-envelope/fork-exec-100x-plan

./ci/compat-envelope/pressure-test.rs run \
  --test backend-parity-c/fork-exec-pipeline \
  --mode verify --backend ptrace \
  --repetitions 100 --cell-timeout 120 --jobs 64 \
  --results ignored/compat-envelope/fork-exec-100x
```

Before a large run, prove a representative fast cell at several widths and
measure achieved concurrency and CPU use, not merely configured `--jobs`.

### Honest M1 status

M1 was **not fully finished** despite several calendar days of work.

It established a much more truthful inner-repository scorecard and fixed several
real harness defects. It obtained the requested series of clean validation
checkpoints across evolving SHAs, one exact full validation of the retained
`1b98ccef...` baseline, and substantial partial repeated evidence. It did not
produce repeated same-SHA full validations, a terminal repeated population, a
validated high-width path, or checked-in red-cell observations.

Do not spend more days polishing M1 infrastructure. Preserve the useful pieces,
make the high-width runner work with a tiny fast test, obtain a bounded terminal
population, keep local validation green, write the exact scoped result, and move
on.

## Milestone 2 — Medium Sanity: strict verify by default, old behavior deleted

### The owner's definition

M2 is exactly three things:

1. strict verify on **all** tests
2. the strictest verify behavior becomes the **default**
3. **completely delete the old behavior**

The third point is the one that repeatedly got skipped. Do not deprecate the
legacy comparator, leave a fallback, hide it behind an environment variable, or
keep it “for one release.” Delete it.

Expect the score to drop. The drop is the measurement of what the old comparator
was hiding, not a regression to tune away. This is one of the rare owner-approved
moments when the green envelope may shrink because the definition of green is
intentionally becoming stronger.

There are no reason buckets or compatibility tiers in this transition. Run the
cell under the strict canonical comparator: green if it satisfies it, red
otherwise.

### Existing M2 implementation candidate

The main body of implementation is already committed at:

```text
3f7eda39a39be157fc4646badf15f4250a05ac84
```

Commit subject: **Make canonical verification the only verification policy**.
It was based on `61b9ba7aa4dd763c1bda091316852d5290f9466c` and is therefore behind current
main. It was independently reviewed, but it is not landed and M2 is not done.
Use it as a concrete source of tested work. Rebase or cherry-pick it onto current
main, inspect semantic conflicts, simplify where appropriate, and finish the
missing DBT work. Do not blindly recreate its roughly 90-file change by hand.

The candidate already does the following:

- removes `--verify-strict` from run and record commands
- removes the lossy `Stripped` comparison mode
- makes bare verification use canonical INFO comparison
- removes unsafe log-diff line filtering and numeric/path erasure
- anchors removal of the real wall-clock prefix at the start of a log line
- preserves payload whitespace and timestamp-like payload text
- returns an error rather than panicking on malformed canonical input
- refuses missing, empty, malformed, output-only, contradictory, or weaker
  verification evidence
- makes verify, record/replay, chaos, validation, and generated commands consume
  typed, matched, non-vacuous canonical evidence
- validates the typed result even when a deterministic guest exit status is
  nonzero, then returns that original status
- removes manifest `assert.bitwise_parity` as an opt-in route around the new
  default
- keeps KVM output-only evidence unqualified rather than pretending it is
  canonical INFO parity
- corrects current docs and skills that recommended deleted flags
- fixes a shell-precedence bug where an L4 `timeout` governed only `mkdir`, not
  the Hermit run and result check

Current upstream `main` before the M2 change still says bare `--verify` is
`Stripped` and recommends `--verify-strict`. That text is stale once M2 lands;
the candidate updates it.

### Real behavior the old comparator hid

An early strict DBT backend-parity run found:

- 23 canonical passes
- 3 declared gaps
- 2 real failures: `process_wait_accounting` and `process_wait_lifecycle`

Those two had matching ordinary output but diverged in scheduler INFO evidence
around wait4/waitid polling, turn counts, and virtual time. This diagnostic run
predated the final DBT evidence hardening, so do not publish its count as the
final M2 score. Its important lesson is valid: the lossy comparator hid real
nondeterminism.

### Outstanding M2 work: trustworthy DBT evidence

The candidate deliberately makes direct DBT `--verify` write `no_result` and
refuse before guest execution. It does **not** keep an unsafe fallback.

Why: the pinned Reverie native client has no evidence descriptor protected from
guest enumeration, duplication, writes, truncation, close, or `/proc/self/fd`
aliases in both full and copied-child paths. A guest-visible file, FIFO, stderr
stream, or raw inherited descriptor can be altered or forged by the program being
verified. That can create an authoritative false green.

Current Hermit pin:

```text
c261050cfd41bec67e31bfd0cf6f56be008d0ebb
```

The then-current Reverie main `ee6716a65d41e8f1d65ee32efa4aafa910b9cf29`
has a byte-identical `reverie-dbt/` tree. A pin-only bump does not fix this.

The following is a source-audited proposed implementation, not a requirement to
preserve one exact fd number if a simpler design satisfies the same protection
contract. Implement the missing Reverie behavior directly in `../reverie`:

- reserve inherited fd 196 for the evidence source; existing reserved fds are
  197/198 and Hermit uses 199
- have `DbtRunner` duplicate an anonymous/unlinked source file to fd 196 before
  exec, clear `FD_CLOEXEC` even when the source is already fd 196, close the
  original child-side alias when it differs, and pass `-evidence_fd 196`
- in the DynamoRIO client, reopen `/proc/self/fd/196` as a DynamoRIO-private,
  descriptor opened for append writes before guest code
- send runtime evidence to that private descriptor; keep ordinary diagnostics
  separate
- hide and protect the inherited and private descriptors in root, forked,
  copied-child, and followed-exec paths
- refuse use of either descriptor as source or target through `dup`, `dup2`,
  `dup3`, `F_DUPFD`, `F_DUPFD_CLOEXEC`, write-family calls, `sendfile`, `splice`,
  `ftruncate`, `close`, and aliases through `/proc/self/fd`
- implement `close_range` by applying normal effects to subranges around the
  protected descriptors
- apply those protections in ordinary pre/post handling, Tool-injected
  `invoke_raw_syscall`, deferred execution, and the direct copied-child path
- filter the descriptor names from `getdents` and `getdents64` in every one of
  those paths
- preserve the inherited descriptor across exec so a new client can recreate
  its private descriptor
- close the private descriptor only after final runtime output

Bracket both directions:

- root, fork child, copied child, followed-exec child, and failed-exec behavior
  all preserve the intended evidence stream
- every forbidden enumeration/mutation attempt fails
- the same syscall operations continue to work on an ordinary guest descriptor
- an injected well-formed frame cannot be accepted as Detcore evidence
- fd 196 survives two execs even if the original source had `FD_CLOEXEC`

The source audit concluded that the callback struct layout need not change, but
the public Rust `DbtRunner` API gains the evidence-file method and the native
client gains `-evidence_fd`. After the Reverie change is committed, advance
Hermit's Reverie revision consistently across the Cargo manifests, both
lockfiles, and the build-budget references in:

- `ci/configure-build-jobs.sh`
- `ci/run-with-reverie-dbt-budget.sh`
- `ci/test_harness.sh`

The prior audit counted 46 Cargo revision/source entries and 16 build-budget
references. Re-derive the count from the exact current tree rather than trusting
the dated number.

Until this is done, the 62 enabled DBT verify cells consist of nine selected
green entries and 53 red/manual entries, and the candidate makes their direct
verification visibly produce no result. That is outstanding M2 work, not a
completion caveat.

### M2 completion sequence

1. Reconcile current Hermit main and the `3f7eda39...` candidate.
2. Implement and test the protected DBT evidence path in Reverie main.
3. Advance Hermit's Reverie pin consistently.
4. Run focused canonical-comparator, DBT, replay, chaos, manifest, and generated
   command tests.
5. Run `./scripts/validate.rs --self-test`.
6. Measure every currently selected M1 green cell once under the new default.
7. Record the exact before/after score. Do not adjust code to recover the old
   number merely because the drop is uncomfortable.
8. Update the manifest plan and `SCORECARD.md` to the strict metric. This is the
   explicit metric transition where cells exposed by the stronger comparator
   may move from green to red.
9. Run a fresh full local validation at the final main commit.
10. Search the repository for the deleted flag, comparison mode, unsafe
    log-filter options, and stale prose. Remaining occurrences should be
    explicit refusal tests or history, not live behavior.

M2 is not “complete except DBT.” A DBT `no_result` is an outstanding blocker,
not an acceptable M2 endpoint, because the owner required strict verification
on all tests. M2 is complete only when DBT produces protected canonical evidence,
the old comparator is absent, the strict score is committed, and local
validation passes against that score, unless the owner explicitly changes the
milestone.

## Milestone 3 — fail closed on unsupported syscalls by default

### The owner's definition

M3 changes the compatibility metric again: unsupported syscalls fail closed by
default. A program must not silently continue by forwarding a syscall for which
Hermit has no policy.

The owner's original wording was: **“Round 3 is full sanity where we also fail
closed by default and fix any tests that depend on unsupported syscalls by
adding those syscalls. This may be a second score drop but we should be able to
ratchet these back up first.”**

The owner expected this to do damage and wanted to inspect that damage rather
than blindly flip the switch. It may be better to expand compatibility after M2
before making the final M3 jump. Measure first; proceed from evidence, and stop
only if the result presents a genuinely material product choice.

### Current syscall categories

Current source exhaustively classifies the pinned x86-64 `Sysno` table:

- 373 pinned named syscalls
- 289 `Determinized`
- 83 `PassThrough`
- 1 `Unsupported`: `restart_syscall`
- no current `Unclassified` variant

Do not use “unknown” as a synonym for all three categories. The distinctions are:

- `Determinized`: Detcore models the syscall or returns an explicit deterministic
  refusal.
- `PassThrough`: Hermit deliberately forwards it under documented assumptions.
  It remains allowed in strict mode; it is not unknown.
- `Unsupported`: normal mode currently warns and forwards; fail-closed handling
  is intended to stop.
- a raw syscall number outside the pinned table is not represented by the
  `Unsupported` enum value. Its treatment is currently backend-dependent.

Classification is not interception. A table entry can say `Determinized` while
the running backend's subscription lets the syscall bypass Detcore. Verify the
running mechanism.

### Current fail-closed behavior

- normal run defaults to `passthru_opt=false`, requests `Subscription::all()`,
  and leaves `panic_on_unsupported_syscalls=false`
- `--strict`, `--panic-on-unsupported-syscalls`, or
  `HERMIT_FAIL_CLOSED=1` enables stop-on-unsupported behavior
- `--passthru-opt` cannot be combined with fail closed
- strict mode still permits all 83 explicitly `PassThrough` syscalls
- `rseq`, keyring calls, and zero-copy pipe calls are `Determinized` families
  whose behavior also changes under fail closed: host passthrough becomes fixed
  `ENOSYS`

`Subscription::all()` does **not** prove “every Linux syscall is intercepted.”
It means all named values in Reverie's pinned table plus CPUID/RDTSC. Current
ptrace filtering explicitly allows `restart_syscall` and `rt_sigreturn` without
tracing and allows raw numbers outside the table by default. The sole named
`Unsupported` syscall may therefore fail to reach Detcore's fail-closed handler
on ptrace.

Backends also disagree on invalid raw numbers. Ptrace normally lets a raw number
outside the table reach the kernel untraced; it can panic only on a path that
actually tries to decode one through `Sysno::from(...)`. DBT directly uses that
conversion. KVM returns `InvalidSyscallNumber`.

Record/replay is separate again:

- its configuration hardcodes `passthru_opt=true`
- it hardcodes `panic_on_unsupported_syscalls=false`
- since `5dde81d0...`, it subscribes all 289 current `Determinized` syscalls
- unlisted `PassThrough` calls such as `chdir` remain unsubscribed and run
  natively

Changing only `Config::default()` will not make record/replay fail closed.

### M3 work

1. Finish M2 first so the comparator is no longer moving underneath this
   measurement.
2. Separate three different controls before measuring:
   - runtime `--strict` / `HERMIT_FAIL_CLOSED=1` controls syscall handling for
     ordinary run
   - the removed M2 `--verify-strict` controlled the comparator, not syscall
     handling
   - record's current `--strict` spelling is parsed but inert, while
     record/replay hardcodes permissive syscall handling
   The retained M1 verify/chaos commands already used runtime `--strict`, so
   merely rerunning them does not measure the default transition. Exercise
   ordinary run without the flag before/after, and wire record/replay into the
   real fail-closed configuration before counting its casualties.
3. Measure the score drop and name every newly red cell.
4. For each new red, distinguish:
   - an intentionally `PassThrough` syscall
   - an `Unsupported` named syscall
   - a raw number outside the pinned table
   - a `Determinized` syscall that bypassed Detcore because subscription was
     ineffective
   - a deterministic fixed refusal such as `ENOSYS`
5. Make interception behavior consistent across ptrace, DBT, and KVM. Test the
   running handler, not just the requested subscription.
6. Fix tests that depend on unsupported syscalls by adding the required syscall
   support. Do not recover the score by relabelling them `PassThrough` or by
   substituting a refusal. Use deterministic refusal only when it is the intended
   Linux feature-absence behavior.
7. Update both syscall-policy sites together:
   - `detcore/src/syscall_classification.rs`
   - the typed dispatch in `detcore/src/lib.rs`
8. Include record/replay; do not leave its hardcoded permissive configuration
   as a hidden exception.
9. Once the damage is understood and accepted, make fail-closed behavior the
   default and remove the normal warn-and-forward path for unsupported calls.
10. Commit the new scorecard and run fresh local validation.

M3 is not “all unknown syscalls were already fixed.” The pinned named table is
classified, but effective interception and raw-number behavior are not yet one
uniform fail-closed policy.

There is old, unlanded reference work at Hermit commit
`769157da8ec0fb3170377a5165443e67c579adc3` with a Reverie dependency at
`e7972364634aae3ef62705527c70a1c0556c5784`. Treat it only as archaeology:
neither commit is current main, and the behavior must be re-derived before any
hunk is reused.

## How to work efficiently

- Use tiny, sub-second guests in the inner loop.
- Ask the manifest/pressure tool for one exact test/backend/mode cell.
- Use seeded samples of ten cells before a full population.
- Share one build across repetitions.
- Let the DAG runner control a fixed worker pool; never run hundreds of jobs
  purely sequentially or launch an unbounded process storm.
- Use the whole machine for independent cell execution after a small scaling
  test proves the selected width. Retain achieved concurrency, wall seconds,
  CPU seconds, peak memory, timeouts, and OOMs.
- Wall time and CPU time are different measurements. Always label which one is
  reported.
- Do not run the full matrix for every code edit. Run focused tests, continue
  coding while an occasional full run executes, then inspect its terminal
  summary.
- A setup/build/sandbox failure before cells is a non-verdict, not a red product
  score.
- Require terminal `summary.json` and `runner-outcomes.json` before claiming a
  complete pressure population.
- For determinism failures, retain the first divergent scheduler turn and
  virtual nanosecond count from canonical `hermit log-diff` JSON.
- Do not cache ptrace golden logs yet. Generate the ptrace reference in the same
  DAG for cross-backend parity until the simple uncached path is solid.
- Do not write durable work only under `/tmp`.

## Things not to rebuild

Do not restart these abandoned directions:

- parent-level canonical scorecard machinery
- compatibility tiers or reason classifiers
- “family receipts” or attempts to make same-permission agents adversary-proof
- receipts for receipts, producer allowlists, or exact-blob freezes around
  validation code
- ownerless deferred-activation/task-closure machinery
- a second validation pipeline that disagrees with Hermit's own manifest
- giant width-four stress runs on a 284-CPU allocation
- claims that same-backend repeatability is cross-backend parity
- claims that configured concurrency is achieved concurrency
- treating an infrastructure non-verdict as either product green or red

The useful outcome of the previous work is product-local: a manifest-derived
scorecard, exact-cell pressure iteration, several real harness corrections, one
substantial partial M1 stability population, and an advanced M2 deletion
candidate. Build from those. Do not reconstruct the coordinator process that
delayed Basic Sanity for days.

## Immediate recommended sequence

1. Fetch current `main`, re-establish the ahead/behind count, preserve
   `0ac2216f...` and `bbafa443...`, and reconcile upstream without resetting them
   away. “Four upstream commits” was only the dated handoff snapshot.
2. Run pressure-runner self-tests and a tiny exact-cell scaling experiment. Fix
   the boxed high-width Cargo denial or reduce the design to a path that works.
3. Obtain one terminal bounded M1 repeated population using real machine
   parallelism; write the exact scoped result and stop M1 infrastructure work.
4. Integrate `3f7eda39...` onto current main.
5. Implement the protected DBT evidence descriptor in `../reverie`, advance the
   Hermit pin, and finish M2.
6. Measure and commit the strict score drop; keep `validate.rs` and full local
   validation passing.
7. Audit and measure the M3 fail-closed transition before changing the default.
8. Make the M3 default change only after the newly red population is understood.

The goal is simple: a `main` tip whose validation is fresh, whose scorecard says
exactly what it measures, and whose green cells are genuinely green under the
current milestone's definition.

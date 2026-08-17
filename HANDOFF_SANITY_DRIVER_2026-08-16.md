# M2 sanity-driver handoff — 2026-08-16

This handoff records the state observed from the owned Hermit slot at
`/home/newton/work/dev-hermit/worktrees/sanity-driver/hermit`. It does not claim
that M2, Hermit PR #2302, Reverie PR #459, or any inspected unlanded ref has
landed.

## Observation boundary

- Observed at `2026-08-16T12:02:32-07:00` on `devbig014.atn7.facebook.com`.
- Owned Hermit worktree: `/home/newton/work/dev-hermit/worktrees/sanity-driver/hermit`.
- Branch: `codex/m2-sanity-driver`.
- Implementation HEAD before this handoff-only commit:
  `39829148def0dd3278e607e33c9885e248a605a6`.
- Task: `sanity-driver-commit-hermit-then-drive-m2`; it remains `in_progress`.
- The primary Hermit checkout was modified only for the owner-authorized Phase
  1 rescue commit `79cc84087904bf7069cef9c585a2eac041341d40`; subsequent Hermit
  work stayed in this slot.
- The Reverie rescue used the owner's primary checkout as the source of the six
  dirty paths. Their before/after content hashes and final status matched after
  the rescue. That proves the owner's observed content was restored; it does
  not prove the checkout was never transiently touched.

GitHub PR state below was queried live at this observation time. If this
handoff-only commit is pushed, #2302's PR head will necessarily move beyond the
implementation SHA above without changing the product code described here.
Re-query both PRs before acting. Build and red-cell claims below come from the
named local evidence and have not been rerun at a post-pin #2302 head.

## Critical path and current pull requests

### Hermit PR #2302

- URL: <https://github.com/rrnewton/hermit/pull/2302>
- State observed: open, draft, not landed.
- Branch: `codex/m2-sanity-driver`.
- Head: `39829148def0dd3278e607e33c9885e248a605a6`.
- Current blocker: `build.workspace` fails because Hermit uses the protected DBT
  evidence APIs `emit_evidence` and `evidence_log_level`, while its checked-in
  Reverie revision is still
  `c261050cfd41bec67e31bfd0cf6f56be008d0ebb`, which does not provide them.

That build failure is a last-observed result, not a permanent property. It is
invalidated by an intentional Hermit Reverie-pin change followed by a fresh
workspace build at the resulting exact Hermit head.

### Reverie PR #459

- URL: <https://github.com/rrnewton/reverie/pull/459>
- State observed: open, ready for review, not landed.
- Branch: `sanity-driver/m2-dbt-protected-evidence`.
- Head: `eae94035ea7313d0b0d1b129a9fbb9c2c30fa3d8`.
- This is the dependency that provides the protected DBT evidence APIs consumed
  by #2302.

No Hermit Reverie pin was advanced in this work. The required order, supplied
by the owner, is exactly:

```text
Reverie #459 lands
  -> owner deliberately advances Hermit's Reverie pin
  -> Hermit #2302 revalidates at the new exact head
  -> the parent Hermit/Reverie/LiteInst2 snapshot publishes
  -> who-am-i@9d38690b activates
```

The parent snapshot publication and `who-am-i@9d38690b` activation were not
independently rechecked from this slot. Treat the sequence above as the current
owner-directed blocking chain, and verify each transition from the running
mechanism before acting on the next one.

`./ci-hub/bin/who-am-i --tag --role impl` currently succeeds best-effort and
returned:

```text
[hermit2, sanity-driver, unresolved, devbig014, role=impl]
```

The older handoff statement that `who-am-i` refuses without inherited
`DG_AGENT_NAME` is stale after the fix at `90a3c5fe`. Always rerun `who-am-i`
immediately before a new commit; the output above is disclosure evidence for
this observation only and will be invalid after identity or activation state
changes.

## Red-cell status

The durable source is
`/home/newton/work/dev-hermit/ai_docs/m2-red-cell-debug-report.md`. It contains
43 populated sections representing 42 unique guest-or-cell/backend/mode
identities.

A 2026-08-16 actionability review found that 15 of the 43 sections rest on
stale or superseded evidence. This is separate from the report's own 33/3/7
section-level outcome accounting: it asks whether a section is a sound target
for work now, not whether it preserves a useful historical measurement. The
15/43 classification must be redone if the report changes, the Hermit branch
changes materially, or fresh exact-head evidence replaces a section.

Only three measured reds remained unresolved after that review:

1. `language-runtimes/node-v8-jit` under ptrace verify: restarted `wait4`
   diverges on host child-reap visibility. One run remains in
   `InternalIOPolling`; the other returns the child PID.
2. `record_rs_sched_yield`: intermittent end-of-run scheduler INFO ordering;
   deterministic DETLOG and COMMIT content agree, but recording can emit the
   final `zero threads left anywhere, fizzling` message twice while replay emits
   it once.
3. The fork/kill/waitpid SIGCHLD control: ordering diverges between asynchronous
   `logically_kill` and completion of the parent's `kill(SIGKILL)`, with later
   runs able to move the boundary into `wait4` child-reap visibility.

These were measured against the rescued dirty-tree lineage ending at
`d601651a3a0f4865b72af50a9c5967eaf60c351a`, not against a successfully built
and revalidated #2302 with the landed #459 pin. Whether each still reproduces
after the pin advance is unverified. Reproduce first; do not treat retained
`target/ignored` evidence as current behavior.

Useful bounded commands and retained evidence are in the red-cell report:

- Node/V8: section `CURRENT ptrace red: language-runtimes/node-v8-jit verify`.
- Fork/kill/waitpid: section `CURRENT ptrace red: fork/kill/waitpid SIGCHLD control`.
- `record_rs_sched_yield`: run
  `cargo test -p hermit --test record_replay record_rs_sched_yield -- --nocapture`;
  the exact-current lifecycle receipt reproduced it twice, but it is
  intermittent.

## Phase 1 rescue and durability

The original dirty Hermit work at `d601651a...` was made durable before the
allocator deleted the live `codex-m2-strict` slot. The rescued content is
contained in `79cc84087904bf7069cef9c585a2eac041341d40`, and the current #2302
head is `39829148...`.

This prevented loss; it did not land the work. PR #2302 is still draft and
unlanded. Do not infer completion from the branch, commits, task tags, or
retained validation artifacts.

## Four inspected unlanded refs

### `ba1eb2ad` — discard as a standalone candidate

It removes pre-Detcore queued-signal virtual-to-host ID rewriting but retains
the `prlimit64` rewrite. Current main still keys the relevant DBT/Detcore state
by host identities, so the commit depends on the broader virtual-identity work
and paired Reverie final translation. Its intent is semantically superseded by
the broader implementation in `79cc8408`/#2302. It requires human review with
that complete boundary, not a mechanical cherry-pick.

### `441c868` — discard as a standalone candidate

It preserves readable `ppoll` output on `EFAULT`, but leaves `poll` unfixed,
depends on earlier event-schema changes, and tests a fully readable local array
rather than a real unreadable tail. The broader #2302 implementation handles
both `poll` and `ppoll`, uses bounded readable-prefix capture, preserves
`EINVAL`, distinguishes timeout-copyout failure, and carries real cross-page
record/replay tests.

### `c2fbd504` — discard as a whole

Its central change promotes KVM strict/verbose verification to internal-log
comparison and claims 25/28 KVM L2 cells. That conflicts with the later
canonical-only direction, which keeps KVM output/status-only evidence
unqualified. The useful lifecycle telemetry INFO-to-DEBUG subset is already in
`79cc8408`/#2302; do not land the KVM L2 promotion without new exact-current
evidence and adversarial review.

### `5bd1b852` — recover only as the full ordered stack, then fix it

This is a Reverie commit, not a Hermit commit. The clean remote source observed
was:

- branch: `origin/research/kvm-rdtsc-403-cherrypick-onto-main`;
- `c560b98309bdbfbf0824dae18797ae38e12a33e9` — route timestamp reads through
  `Tool`;
- `7ca564e8682628b1bfbf072d028a84bd77500ee8` — continuous timestamp evolution
  coverage;
- `5bd1b852ad2c649a5fd0c11f261053cbd2be903c` — subscription guard and
  unsubscribed-RDTSCP refusal coverage.

The range changes only five `reverie-kvm` paths. Its base
`ee6716a65d41e8f1d65ee32efa4aafa910b9cf29` was the direct parent of observed
Reverie main `4f57671df96fa0b499e0925d449a50b332e369a6`; main's intervening change
touched only policy paths, and read-only merge analysis found no conflicts.
This applicability result is invalid if Reverie main advances or any of the
five KVM paths change; re-run containment and merge analysis before recovery.

If the owner authorizes recovery on a branch based at the rechecked Reverie
main, the prepared one-command range is:

```bash
git cherry-pick c560b98309bdbfbf0824dae18797ae38e12a33e9^..5bd1b852ad2c649a5fd0c11f261053cbd2be903c
```

Do not run it without owner authorization. The recovered range is not ready to
validate or land as-is because later exact-head review found a blocking defect:

- a `ThreadOwnership::Host` `CLONE_THREAD` worker inherits `CR4.TSD`;
- that worker runs the tool-less KVM loop, which has no RDTSC recognizer;
- its first CPL3 `RDTSC` therefore terminates with `#GP`.

The recovery must first settle the existing Host-owned-thread behavior, fix
the interception/dispatch mismatch, and add a regression that actually uses
`ThreadOwnership::Host`, subscribes to RDTSC, creates a worker, and executes
RDTSC. Existing lifecycle coverage uses Tool-owned workers; existing Host-owned
coverage does not subscribe to RDTSC.

Historical #403 validation does not carry forward. A repaired exact head needs
at least:

```bash
cargo test -p reverie-kvm --all-features -- --test-threads=1 --nocapture
cargo clippy -p reverie-kvm --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
./validate.sh
```

All six timestamp integration tests, the decoder-boundary unit test, and the
new Host-owned regression must actually execute. To retain the prior integrated
claim, rerun the strict-KVM `/bin/true --detlog-stack` measurement for three
qualifying runs and require 31 hashes/run with all three pairwise comparisons
31/31. The old 229/0 Reverie result and 31/31 Hermit result are historical,
not exact-head evidence for a repaired stack.

## KVM host limitation

At the observation time this host was `devbig014`, Linux `6.19.2`, x86-64.
AMD virtualization and KVM modules were visible, but `/dev/kvm` was absent.
Therefore this host cannot perform the KVM validation above.

`reverie-kvm` tests use `kvm_available()` and return `ok` early for `ENOENT`,
`EACCES`, or `EPERM`. A green KVM package suite on this host would be a false
green unless the output proves that the KVM tests executed rather than skipped.
Working CPUID faulting does not establish KVM availability.

## Do not use the contaminated PR #403 branch

Do not recover from `fix/kvm-deterministic-tsc-stack-hashes`. GitHub showed the
closed, unmerged PR #403 at head
`4c441762d8a38cba1b8f8ad517ed0dc1ca65e1fd` with six commits:

- three RDTSC commits;
- `8766e512df7d8b3eda61a44706b00b69e2a5946a` and
  `6ee609dd031cf9ec24800759864e779dfd8bfadc`, unrelated static-image heap-arena
  work in `reverie-kvm/src/elf.rs`;
- `4c441762d8a38cba1b8f8ad517ed0dc1ca65e1fd`, unrelated SCM_RIGHTS
  endpoint-identity test changes in `reverie-kvm/src/executor.rs`.

Its recorded validation was bound to earlier clean head `a6aa8bc4...`, not to
the final six-commit artifact. The PR was closed without landing. Use the clean
three-commit remote range above, after owner authorization, and still fix the
Host-owned-thread defect before claiming a green result.

## Resume order

1. Re-query Reverie #459 and Hermit #2302; do not assume the heads in this file
   are still current.
2. Wait for #459 to land. Do not advance Hermit's Reverie pin before the owner
   explicitly chooses the landed revision.
3. After the owner advances the pin, build #2302 at its new exact head.
4. Re-run the three unresolved red reproductions before changing product code.
5. Run focused validation, then the applicable exact-head Hermit validation and
   review. Evidence under `target/ignored` is supplemental, not landing.
6. Publish the parent snapshot only after its exact Hermit/Reverie/LiteInst2
   gitlinks are deliberate and validated.
7. Verify the running `who-am-i` consumer before claiming `9d38690b` activated.
8. Keep the task `in_progress` until the required changes land and fresh
   ancestry confirms them. `implemented` is not landed.

Do not advance the pin, cherry-pick the RDTSC stack, modify a primary checkout,
or close the task merely because this handoff and its source branches are
durable.

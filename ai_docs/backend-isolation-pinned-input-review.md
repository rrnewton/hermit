# Independent pinned input transport review

Hermit source base: `5a3fdbb3c80a6f1673d8a51f617c660ffa2e6ca3`.
Documentation base: `bc4d5d91edeaf01b95a2cbd2eff2b488a45ef379`.
Reverie prerequisite base: `255d2e0b8f5ba017bcc7233711c63583e4fe419e`.

Review target: `backend-isolation-pinned-input-amendment.md`, SHA256
`4da70ff783a6e3bf86c9ea50fa0c04183a1729d2516df721daaf5a764587be08`.
The original design remains frozen at SHA256
`9c4e889debd7bc7192b049c6315167cb11b2ea9311d45911aa4c886b1f6bc7d8`.
This attestation approves the additive design only. A later byte change needs a
new review; product code, the utility prerequisite and conformance tests are
not approved by this document.

## Findings and ownership

The proposed direct bind of a pre-clone O_PATH descriptor failed before guest
entry. Native probes showed its mount identity absent from the cloned tree;
both bind and open_tree returned EINVAL. Reopening the same lexical source in
the child's namespace produced the same held object and permitted its bind.
This corrects an infeasible transport claim in the original design rather than
declaring previously successful cwd or symlink inputs unsupported.

The amendment specifies a checked operation in the existing ordered mount
loop. A held original descriptor prevents inode reuse. The child compares its
reopened object, type, full fstatvfs flags and raw symlink bytes, establishes
current mount membership, binds that descriptor and verifies the actual
destination. The target type comes from the held object, avoiding the existing
[procfd lstat typing defect](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/mount.rs#L440).
No unchecked pathname fallback, copied object or broad sibling-directory bind
is approved.

The actual owned launcher calls
[clone_with_stack_owned](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/clone.rs#L152),
which rejects non-Namespace bits and supplies only namespace bits, SIGCHLD and
CLONE_PIDFD. Its memory and descriptor tables are private even with an empty
namespace set. The
[invalid-flags checks](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L3344)
cover VM, FILES, FS, SIGHAND, THREAD, VFORK and PARENT sharing. The amendment
also requires pinned entry points to reject memory or descriptor sharing before
clone. A close-once state therefore changes only the child's ownership.

All Mount-held clones must share that state and be retired before logger,
runtime, evidence-listener or backend-worker startup. Retiring one caller
reference while a Mount retains a live source is insufficient. Close failures
must not lead to retrying a descriptor number that Linux may already have
released. Parent ownership continues through actual reaping, including a
retained unresolved owner. Constructor/equality behavior, incompatible builder
flags and every success/error retirement path require exact-diff review.

## Determinism conclusion

The amendment changes host setup object transport. It leaves Detcore,
continuous virtual time, scheduling, logical guest IDs, raw Unix statuses,
statistics, stdin/TTY behavior and strict evidence comparisons unchanged.
Setup failures remain failures. Each physical verify run still needs its own
owned setup and evidence; a successful native setup probe is not L1 or L2.

The post-clone operation must use prepared buffers and raw operations without
allocation, deallocation, tracing or blocking locks. Recursive input mounts
must retain every actual submount and policy. An explicit read-only seal must
preserve locked attributes, and destination observation must prove the intended
flags rather than accepting a successful mount syscall alone.

## Linux and POSIX conclusion

The amended mechanism retains actual objects and symlink bytes, ordinary Linux
parent/symlink resolution and argv0. Default cwd below private tmp, plain
parent-relative programs and sibling symlink programs remain successful
required cells. Explicit workdir and user mounts retain precedence. Replaced,
missing or policy-changed inputs refuse before workers; input capabilities are
retired rather than merely marked CLOEXEC.

The native v3 probe source SHA256 is
`2d3c79be24349a5f97e2144a308da2fac07b6b89d3bcfac77cdbcf76c6732741`;
its binary SHA256 is
`e70a5342cd04bd0673f17fed82fdd1f1c9eca2eea8dad663d81d654e731a72e7`.
Independent inspection confirmed compile/run exits 0, identical
original/reopened/bound object identities and fstatvfs flags for directory,
file and nofollow symlink, exact link bytes, child descriptor retirement and
parent pins surviving actual reap. Missing/replaced and same-inode read-only
alias controls retained their refusal predicates. These are native mechanism
controls, not proof that the unimplemented utility or repaired Hermit meets
the contract.

## Evidence and goalpost-moving conclusion

> GOALPOST-MOVING REVIEW RULE
>
> Adversarial reviewers must look explicitly for goalpost moving. We are extremely skeptical of any goalpost moving. YOU DO NOT CLEAR THE BAR BY SIMPLY LOWERING THE BAR.
>
> Treat each of these as an explicit review target:
> - weakening an assertion so a test passes
> - widening a tolerance · adding an exemption · skipping a case · relaxing a comparator
> - renaming or relabelling so a failure reads as a pass
> - deleting a check rather than satisfying it

- **Assertions weakened: no.** Successful default cwd, parent-relative and
  sibling symlink launches are preserved. Their observed migration failures
  remain blockers until repaired; universal refusal was rejected.
- **Tolerance widened, exemption added, case skipped or comparator relaxed:
  no.** Required namespaces, filesystem and network observations, all backend,
  root and mode cells, literal guest FD numbers and capabilities, raw failures,
  mount rows, statistics and strict verification remain required. No runtime FD
  row is exempted by ownership labels.
- **Failure relabelled as a pass: no.** Native probe success establishes only
  feasibility. The limited DBT launch test must fail the original baseline and
  succeed on repaired source; the full report can remain failing. KVM missing
  guest observations, emulated namespace readlinks and observer no_result rows
  retain their existing classifications.
- **Check deleted instead of satisfied: no.** The amendment preserves the
  complete conformance report and requires new live transport controls. It
  changes the failed implementation mechanism rather than deleting its cells.

## Review triggers and residual risks

As described, a reverie-process ownership handle and Mount constructor are
utility APIs. They do not change Tool, Guest, Backend or the syscall interception
model, so they do not meet trigger 2. Transporting real pinned objects before
guest execution adds no determinization strategy, so it does not meet trigger
3. There is no new syscall support or scheduling change under triggers 1 or 4.
The actual prerequisite diff must independently confirm this classification.

The prerequisite needs its own exact-base code review and focused evidence.
No dependent Hermit pin may name an unpublished, unreachable commit. Current
migration source remains unapproved. Required live ordinary/verify input
controls, all-proc source retirement, signal/reaping behavior, full descriptor
parity, closed-stdio handling, KVM guest view and the complete mode/root matrix
remain outstanding. No observer predicate change or new FD strategy is
authorized by this amendment.

## Verdict

**Approve and freeze the exact additive design digest above.** Implementation
and overall backend conformance remain unreviewed and incomplete.

# Pinned input transport amendment

This is an additive amendment to the isolation contract with SHA256
`9c4e889debd7bc7192b049c6315167cb11b2ea9311d45911aa4c886b1f6bc7d8`.
Approval is established by an exact-digest adversarial attestation in a separate
review record. Implementation requires that approval. The approved original
document remains unchanged.

Hermit source base: `5a3fdbb3c80a6f1673d8a51f617c660ffa2e6ca3`.
Local documentation base: `bc4d5d91edeaf01b95a2cbd2eff2b488a45ef379`.
Reverie prerequisite base: `255d2e0b8f5ba017bcc7233711c63583e4fe419e`.

## Measured defect and scope

The original filesystem paragraph proposed binding a pre-clone O_PATH handle
through `/proc/self/fd/<N>` after cloning the mount namespace. That handle
retains the original vfsmount, which is absent from the child's cloned mount
tree. Linux refuses the bind with EINVAL even when the target has the correct
type. Native old-handle `open_tree` attempts also refuse EINVAL. Successful
ordinary DBT launches with a default cwd under private tmp therefore regress
before guest entry in the held migration patch; compile and unit checks alone
did not establish that launch policy.

This amendment changes that transport mechanism, preserving the original
input objects, capability retirement, parent reaping ownership and all
conformance requirements. It does not change scheduling, virtual time, syscall
emulation, guest configuration, evidence comparisons, namespace policies or
the set of supported cwd/input cells.

Native controlled probes compiled with `cc -Wall -Wextra -Werror` and ran
through the bounded resource wrapper. These are setup feasibility experiments,
not product conformance or determinism passes.

| Probe | Source SHA256 | Binary SHA256 | Observations; compile/run exits |
| --- | --- | --- | --- |
| Cross-namespace object transport | `8374d14f2ee025755c72f61ef425eb169d2cce8da04b4d8b26aa038de081a0cb` | `5410c90ac54e72baa184f3e26b76b263c1561c2621cbb80c19b94da4a3c1b31a` | Old directory/file/symlink handles bind and open_tree EINVAL; child lexical repins bind successfully; 0/0. |
| Namespace membership and ownership | `9c68b2adf8710759fee8b0bd15f82fcb367eea1996689569b9df3dde40045e9b` | `5d8f50161923b764e8dbc70fc411817bab8f08271df3a5421084a9e20df2be72` | Old mnt_id absent, repinned mnt_id present; exact object/type; symlink bytes; missing/replaced refusal; child retirement; parent pins survive actual reap; 0/0. |
| Mount policy and destination | `2d3c79be24349a5f97e2144a308da2fac07b6b89d3bcfac77cdbcf76c6732741` | `e70a5342cd04bd0673f17fed82fdd1f1c9eca2eea8dad663d81d654e731a72e7` | Original/reopened/bound fstatvfs flags equal; direct bound symlink bytes equal; same inode through a read-only alias refuses; previous controls retained; 0/0. |

Raw observations, source snapshots, compiler exits, wrapper invocation,
stdout/stderr, exit and resource receipts are retained with the private
handoff. No experiment sources or binaries enter the product repository.

## Existing setup paths cannot transport these pins

Pinned Reverie applies
[mounts before chroot and chdir](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L715)
and runs
[pre_exec callbacks after those operations](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L753).
Its physical-run startup callback is also after setup. Moving input creation
there would lose the sources already hidden by tmp/root mounts and could fail
chdir first. Literal pathname binds can resolve a replaced object and follow a
symlink instead of binding its pinned link object. Subsequent identity checking
does not restore the missing symlink-object operation.

The existing [touch_target implementation](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/mount.rs#L440)
uses lstat of its source path. On a procfd magic link, that describes the proc
link, so a directory source incorrectly receives a file placeholder. A checked
pinned operation must derive target type from fstat of the actual pinned object.

## Minimal utility API

Add one opaque, clonable host-side ownership handle and one Mount constructor
in reverie-process. This is not a Tool, Guest, Backend or interception API.

```rust
pub struct PinnedMountSource { /* shared owned source and captured identity */ }

impl PinnedMountSource {
    pub fn open(source: impl AsRef<OsStr>) -> Result<Self, Errno>;
    pub fn is_dir(&self) -> bool;
    pub fn retire(&self) -> Result<(), Errno>;
}

impl Mount {
    pub fn bind_pinned(
        source: &PinnedMountSource,
        destination: impl AsRef<OsStr>,
    ) -> Self;
}
```

`open` runs in the parent before clone. It opens
`O_PATH|O_NOFOLLOW|O_CLOEXEC`, captures device/inode/type and the complete
fstatvfs mount-flag value, and records an absolute source spelling without
collapsing Linux symlink/parent semantics. For a symlink it captures the link
object and exact raw readlinkat bytes, not its referent. The caller separately
declares the referent objects needed by its program input graph.

The handle owns its descriptor through a shared close-once state. No
std::File or OwnedFd simultaneously owns the same raw descriptor. Mount clones
share that state; `retire` closes its current process's owned copy exactly once
and marks every local clone retired. Fork memory independence preserves the
parent copy. The ordinary last-owner destructor closes an unretired descriptor.
No public borrowed descriptor outlives retirement, no retry can close a reused
number, and retirement is not a CLOEXEC substitute.

The actual common owned route uses
[clone_with_stack_owned](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/clone.rs#L152):
it rejects every bit outside Namespace::all and supplies only namespace bits,
SIGCHLD and CLONE_PIDFD. Namespace contains only the seven NEW* flags. There is
no CLONE_VM, CLONE_FILES, CLONE_VFORK, CLONE_THREAD or shared-parent flag, including
when no namespaces are selected. Its
[invalid-flags regression](https://github.com/rrnewton/reverie/blob/255d2e0b8f5ba017bcc7233711c63583e4fe419e/reverie-process/src/container.rs#L3344)
already checks these refusals. The real owned child therefore has COW memory
and its own descriptor table; source retirement cannot mutate parent ownership
or close its descriptor.

Pinned mode requires that same private-memory/private-descriptor premise.
Applicable Container/Command entry points must reject any requested
CLONE_VM/CLONE_FILES/CLONE_VFORK/CLONE_THREAD sharing before cloning when pinned
mounts are present; legacy unpinned flags retain their existing behavior. Do not
make a parent-visible close-state change from a shared-memory child or silently
close a shared descriptor table. Mount setup and retirement remain in the
single-threaded physical entry, before workers; no concurrent retirement is an
accepted launch configuration.

The constructor retains the lexical source for diagnostics and get_source.
Legacy pathname Mount builders and equality behavior remain unchanged. Pinned
mode accepts bind, recursive-bind and an explicit read-only seal; conflicting
source, filesystem, remount/move or propagation requests refuse explicitly.
Root propagation isolation stays a separate preceding operation.

## Ordered checked bind

Each pinned operation executes in the normal ordered Mount::mount loop,
after namespace creation and identity mapping but before any mount that hides
that source. It is selected before setup; it is not an error fallback.

1. Require a live parent pin with the captured object identity and mount flags.
   Reopen the same absolute lexical source in the child with
   O_PATH|O_NOFOLLOW|O_CLOEXEC. Missing or changed sources refuse.
2. Check reopened device/inode/type and complete fstatvfs flags against the
   authoritative held object and parent snapshot. For a symlink, require exact
   original/reopened readlink bytes. A same-inode alias with changed
   readonly/nosuid/nodev/noexec/atime policy refuses.
3. Establish that the reopened descriptor's mount belongs to the current mount
   namespace. Retain raw mnt_id evidence in tests; independent namespace
   launches never compare literal numeric mount IDs.
4. Create a directory target only for a pinned directory; otherwise create a
   file placeholder. Do not type the procfd magic link. All targets are inside
   the owned backing tree and are prepared before installing guest input
   symlinks. Existing incompatible targets and redirected target parents refuse.
5. Bind from the verified child descriptor through its procfd. The kernel uses
   that held object even if the lexical pathname is renamed afterward. Never
   reopen a pathname for the mount itself. Recursive directory binds preserve
   their actual submounts and policies. Apply any explicit read-only seal with
   locked flags preserved.
6. Open the actual destination with O_PATH|O_NOFOLLOW. Require its exact
   authoritative device/inode/type, symlink bytes when applicable and mount
   flags, including only the explicitly requested read-only change. A failed
   or mismatched observation refuses before workers; it never accepts a
   raced replacement or substitutes another object.
7. Close the temporary child-reopen/destination handles on every success and
   error path. Original source handles remain owned until common child
   retirement immediately after setup and before logging/runtime/backend work.

Reopen and verification use raw host operations on stack/prepared buffers with
no allocation, deallocation, blocking lock or tracing inside the post-clone
mount loop. Source CString, symlink bytes, expected identity and any scratch
capacity are prepared before clone. Unsupported namespace/mount identity
observation refuses; there is no unchecked or literal-path fallback.

An inode held throughout prevents inode reuse from satisfying the comparison.
Lexical changes before reopening refuse; changes after reopening cannot change
the FD-bound object. Destination validation proves which object was actually
installed. This faithfully retains pin semantics rather than accepting a new
pathname object.

## Common Hermit ownership and input graph

ContainerInputs owns the source handles in the existing owned-run guard value.
Mounts retain handles only for checked setup. Common child work calls retire
on every source and drops its handle copies before any logger, Tokio runtime,
evidence listener or backend worker. All Mount references then describe retired
handles, so none keeps an alternate source capability alive. Parent guards
remain until actual child reaping or remain retained with an unresolved owner.
Setup errors use the existing typed setup failure and reaping path.

Preserve default cwd strictly below tmp and executable/interpreter inputs hidden
by automatic tmp, with exact tmp itself still fresh. Explicit completed-root
workdir and user mount/tmp intent remain authoritative; no unsolicited caller
subtree is preserved for an explicit cwd. Allocate owned scratch, identity and
statistics material outside any preserved cwd, including when TMPDIR names it.

The program input graph retains actual intermediate symlink objects needed to
resolve the original pathname and argv0. For example, caller tmp/P/cwd running
`../alias/guest`, with tmp/P/alias pointing to tools, declares that link object
and the actual tmp/P/tools/guest file. It does not bind the entire sibling
directory. Precreate all logical targets before installing links; use checked
nofollow pins for the links themselves. Keep Linux parent/symlink resolution
for ordinary cwd/program inputs while leaving the existing e9 mapping safety
normalizer and its assertions unchanged.

## Required evidence and landing order

Keep the full conformance report, every required backend/mode/root/input row,
all counters and the strict comparator. The limited first-slice DBT test
continues to fail the source baseline for PID/UTS/PID1/tmp and must pass repaired
source. The full isolation report may still contain existing mismatches and
no-result cells; none becomes an exemption.

Require live ordinary DBT and physical verify controls for default cwd below
tmp, plain parent-relative program, sibling symlink program, explicit visible
cwd with hidden irrelevant caller, relative workdir, parent/symlink-parent cwd,
absolute tmp input, hidden interpreter/env input and TMPDIR within preserved
cwd. Require changed/missing/mount-policy input refusals, exact source object
aliases, target typing, child retirement and parent lifetime through reaping.

Implement and review the Reverie utility prerequisite independently at its
pinned base. Report its full commit SHA, base, exact paths/checks and dependent
Hermit evidence to the coordinator. Do not advance the Hermit gitlink to an
unpublished prerequisite. The coordinator must arrange reachable publication
and pin sequencing before dependent Hermit work is handed back as landable.

This utility transport introduces no syscall emulation, new determinization
strategy or scheduler change. Review must confirm that classification against
the actual diff. Existing KVM guest-view, runtime FD, closed-stdio, namespace,
filesystem and mode gaps remain required separate work. This amendment resolves
neither those gaps nor overall backend conformance.

Issues: https://github.com/rrnewton/hermit/issues/4009 and
https://github.com/rrnewton/hermit/issues/4002.

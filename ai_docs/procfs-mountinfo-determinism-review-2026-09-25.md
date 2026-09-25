# Deterministic-scheduling review: guest mountinfo membership (procfs)

Base: `422f3f3a4e05353edd4f2449affc8df9241bdf51` (exact, fetched 2026-09-25).

## Grounding order (skill §0–§1)
- ASPLOS paper (Kaiser et al., external ACM link): unreachable from this host
  (no direct egress; pane allowlist excludes curl) — no design claim depends
  on it; normative sources below are local and primary.
- `detcore/src/scheduler.rs` at base (read: deterministic core, timed events,
  RCB/committed-time, run-queue/deadlock rules) + `DETERMINISM_ARGUMENT.md`
  (causality scope, entropy inventory) + `PROJECT_VISION.md` (deterministic
  record & replay as canonical interface) + v2 roadmap guest-semantics notes.
- Linux: `/proc/<pid>/mountinfo` (proc(5)): mount ID, parent ID, major:minor,
  root, mountpoint, options, optional shared/master/propagate_from peer
  groups, ` - ` separator, fstype, source, super options. Shared/slave
  propagation imports peer-group mounts into a namespace asynchronously.
- Full affected path read: `detcore/src/syscalls/files.rs`
  (`snapshot_procfs`/`initialize_procfs_snapshot`, fdinfo tracer snapshot),
  `detcore/src/procfs.rs` (`MountInfoSnapshot`, `sanitize_mountinfo`,
  `sanitize_fdinfo`, `syscall_time`), `detcore-model/src/procfs.rs` parsers,
  `hermit-cli/src/lib.rs` producer provenance capture.

## Frozen semantics / design selection
Guest mount namespace under Detcore is a launch-defined, immutable object:
files.rs already records "Mount/unshare/setns are refused once Detcore
starts". Membership of the deterministic guest view is therefore the launch
namespace minus rows that are chosen determinism fidelity trade (real propagated mounts, excluded for determinism scope):

**Excluded class** — `fuse.squashfuse_ll` mounts under `/mnt/xarfuse/`:
per-process ephemeral seed mounts created by host squashfuse infrastructure
(mountpoint embeds uid + host cgroup PID), imported by shared propagation,
lifetime bound to unrelated host processes. This is the PID-virtualization
analogue for mounts: other tenants' runtime state. Everything else stays:
kernel/system rows, shared filesystems (edenfs/manifold/btrfs), binds,
overlays, tmpfs, and every Hermit-configured mount (`--mount`, container
provenance, `/test`, `/tmpvol/.hermit/*`).

Alternatives rejected: launch-only full capture (does not fix cross-run
churn — the failure is between two invocations); synthesized fixed table
(hides legitimate guest-visible shared mounts); comparator/guest change
(weakening, forbidden).

Applied at every membership consumer: procfs capture (`initialize_procfs_
snapshot`), fdinfo tracer snapshot, and producer provenance
(`capture_mountinfo_identity_order`) — otherwise seed rows would still
shift run-global mount-ID assignment or captured identity order.

## Event path / lifecycle / backends
open/read of mountinfo → first-read capture (class-filtered) → snapshot
identities assigned over filtered membership → seqfile reads render from the
same filtered contents. dup/fork share the open-file snapshot (unchanged);
exec creates no new namespace under Detcore (refusals unchanged);
record/replay: capture is a host read at first use in both modes and the
filter is a pure function of contents, so replay renders identically.
Backends: ptrace measured end-to-end here; liteinst/sabre share Detcore
procfs code (claims limited to ptrace — no liteinst/sabre cell changes);
kvm path uses the same CLI provenance capture. Guest mount changes remain
visible exactly as before: Hermit launch/configured mounts and all
non-seed host rows (regression test asserts `/test`, edenfs, binds survive;
unit test asserts non-seed squashfuse and non-fuse xarfuse rows survive).

## Evidence
- Unit: `ephemeral_host_seed_mount_class_is_precise`,
  `seed_churn_does_not_change_guest_mountinfo_membership` (old leak fails:
  churn variants differed before, identical after, through sanitize).
- E2E: guest `cat` view 101→86 rows, 0 xarfuse, `/test` + edenfs retained.
- Cell: `system-utils/procfs-sanitized-paths` canonical verify **20/20
  matched** under its manifest profile (no relaxations); cell re-enabled
  with pinned evidence (`PROCFS_MOUNTINFO_2026_09_25_*`).

## Mounts view
`/proc/<pid>/mounts` now carries the same exclusion (mounts grammar, ProcfsKind::Mounts), so the two guest views agree. KVM reverie-kvm proc_mounts capture is pre-existing and unmeasured. A retained row whose parent is an excluded seed still snapshots: the constructor's parent pass covers it and its parent id is rewritten deterministically (unit-tested, `retained_row_with_seed_parent_still_snapshots`).

Residual: a guest program that itself drives host squashfuse seeds would
not see its own post-launch seed mounts (they were never deterministic —
host-assigned hashes/PIDs); disclosed in the PR. The one `not_run` from the
retained 18/20 is infrastructure (canonical NotRun stamp), not product.

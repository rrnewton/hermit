# Slot images and slot sandboxes (proof of concept, 2026-09-28)

Status: working design with a working implementation on branch `slot-images`. Settled
user-facing text lives in `common/docs/wrkslots/USER_GUIDE.md` ("Disk-image slots" and
"Running commands and agents inside a slot's box").

## Problem

Agents working in long-lived slots on one large development host write enormous numbers of small
files (checkouts, build trees, caches) straight into the host file system, and a runaway process
can fill the host. Two host failure modes follow:

1. **Bytes.** One process writes until the device is full.
2. **Metadata.** On a copy-on-write file system with a doubled metadata profile, millions of
   small files consume metadata block groups. Once the device has no unallocated space left for a
   new metadata chunk, the file system aborts transactions and turns read-only, while `df` still
   shows hundreds of GB free inside data chunks.

A per-run btrfs subvolume with a qgroup limit bounds bytes, but it requires quotas on the whole
host file system (every write on the host then pays qgroup accounting, and large deletions
trigger rescans) and root to delete each subvolume; a caller that loses privilege leaks one
subvolume per run.

Constraints from the owner:

- No persistent host changes: no repartitioning, no mount options, no enabling quotas, nothing a
  configuration manager would revert or a fresh host would lack. A short-lived `sudo -n` helper is
  acceptable; an unprivileged fallback is wanted.
- No pre-reservation: a slot has no natural "full size".
- Representation must not change lifecycle policy (ownership, salvage, reclaim).
- Existing slots must keep working when the default changes, with an in-place migration.
- Builds should keep final outputs in the slot but leave intermediates in an action cache outside
  it.

## Design

### Storage: one sparse ext4 image per slot

`<control>/slot-images/<type>/<slot>/{IMAGE.json, slot.img, state.img, state/}`. `slot.img` is
mounted at the slot directory; `state.img` holds the sandbox's per-slot private `$HOME` layer.

- Images are created no-copy-on-write (`chattr +C` on the empty file) so a copy-on-write host
  stores each image as a few large extents, not one extent per guest write, and are sparse:
  `ceiling_bytes` bounds the apparent size and reserves nothing.
- `mkfs.ext4 -m 0 -J size=64 -E root_owner=UID:GID,nodiscard,lazy_itable_init=1`. A fresh image
  costs about 70 MiB of host space.
- Kernel mounts use `-o loop,discard`, so deletions punch holes in the backing file. Measured on a
  100 GiB image: 518 MiB allocated after mkfs (default journal), 2.6 GiB after writing 2 GiB,
  530 MiB after deleting it. FUSE mounts return space on `fstrim` (measured 1.1 GiB back to
  531 MiB).
- Backends: `kernel` (`sudo -n mount -o loop`) and `fuse` (`fuse2fs`, run in a transient user
  service so it outlives the command that mounted it). Creating 50,000 empty files took 1.47 s on
  the kernel mount; 5,000 took 1.63 s on FUSE, so FUSE is roughly ten times slower for
  metadata-heavy work.
- Every configuration load remounts image-backed slots that are not mounted, at the location
  recorded in `IMAGE.json`.

Lifecycle integration points in `cli.py`, all behind helpers that fall back to the old behavior
for plain slots:

| Point | Plain slot | Image slot |
|---|---|---|
| create | `mkdir` (nested layout) | provision images, mount at the slot path |
| path fence and its rollbacks (10 rename sites) | `os.rename` | unmount, rename the empty mount point, remount |
| remove fenced/aborted slot directory | `rmdir` | refuse unless only mkfs residue remains, then destroy the images |
| flat-layout `git worktree remove` | normal | accept Git's expected EBUSY on the image root after it deleted the content and the administrative entry |
| process-use census | any mount inside the slot is use | the slot's own image mount lines are excluded |

Representation is per slot: a slot is image-backed exactly when its `IMAGE.json` exists. Nothing
was added to the registry records, so the replay schema is unchanged.

Found and fixed during testing: after `fusermount -u`, the kernel detaches the mount before
`fuse2fs` has written its cached metadata. Remounting immediately started a second server that
read stale metadata, and a file deleted just before the path fence reappeared, which the salvage
check then correctly reported as "changed after salvage". Unmount now waits for the old server
to exit, and mount refuses while one is still running.

### Migration

`wrkslots image set-default image|worktree` changes only new slots. `wrkslots image convert SLOT
--to image|worktree` copies an idle slot (no process using its path), compares full listings
(path, type, size, mode, mtime), swaps at the same path, and re-verifies Git worktree identity.

### Sandbox: `wrkslots run SLOT -- COMMAND`

The sandbox is orthogonal to the representation: plain-worktree and image slots get the same
limits and the same view. Representation only decides where the slot's private state lives
(`state.img`, or `<control>/slot-state/<type>/<slot>/`). Revised on 2026-09-28 after owner
review and a root-launcher acceptance run against real harnesses.

- **Limits.** A transient systemd user scope for the calling process itself, in
  `<enclosing>-wrkslots-<slot>.slice` (created through the user manager's D-Bus API with the
  caller's PID, then exec). The slot slice is nested inside whatever slice the caller already
  occupies, so a site policy that confines a caller to its own slice is never escaped.
- **PID stability.** Every step execs; the PID never changes. A terminal multiplexer that only
  starts agents when the pane's own shell PID is the foreground process (Herdr checks exactly
  this) accepts the boxed shell. `wrkslots shell-command` prints such a command line with absolute
  interpreter paths, because a forking interpreter shim anywhere in the chain breaks the check.
- **Isolation modes.** `userns` (unprivileged user + mount namespace), `root` (the identical view
  built through `sudo -n` in a plain mount namespace), and `cgroup` (limits only). One view
  builder (`sandbox.build_view`) serves both view modes. `root` exists because a harness launcher
  that performs a setuid step fails inside a user namespace. The root helper switches to the
  user's uid/gid/groups immediately, keeping only `CAP_SYS_ADMIN` (`PR_SET_KEEPCAPS`, `capset`),
  so the view is built with the user's own path access. A root process cannot reach a FUSE mount
  the user owns, such as a `fuse2fs` slot image. It then drops the capability and execs; `sudo`
  inside the box works and `CapEff` is 0. The environment travels in a private spec file (sudo
  scrubs it), and the scope is entered before sudo so the cgroup is inherited.
- **Root mode and Herdr.** sudo forks, and since sudo 1.9.14 `use_pty` is on by default: the
  pane's shell PID becomes `sudo`, and the boxed program runs on a private pty behind a sudo
  monitor. Tested in a real pane: `herdr agent start` refuses (`agent_pane_busy: ... is not an
  available shell`), `herdr agent prompt` refuses, and Herdr never detects a harness there by
  itself. Avoiding this needs sudoers changes (`!use_pty`, `!pam_session`), which count as host
  customization. What works: `pane report-agent` labels the pane, after which
  `herdr agent explain` evaluates the bundled screen rules live (codex: idle -> working -> idle
  through the relay). The reported state itself stays whatever was reported. Claude keeps its
  prompt box on screen while working, so its rules read idle throughout. The "esc to interrupt"
  hint is the reliable working signal for both harnesses. A dead end, reported by the coordinator:
  `setsid sudo` plus a root TIOCSCTTY steal of the pane pty makes the harness the tty's foreground,
  but the pane shell permanently loses its controlling tty.
- **agentctl `herdr-relay` adapter** (both editions). For root isolation, agentctl runs
  `wrkslots shell-command SLOT -- HARNESS ARGS` in the new pane. It pins the relayed program (the
  first process below the pane's shell PID that belongs to the user and shares its slot scope,
  found through world-readable `/proc/*/stat` parent links, because sudo's own task directory is
  unreadable) and reports the pane as the harness. The state is `agent explain` plus the interrupt
  hint; a changed state is reported back so Herdr's listing stays current. Prompts go through
  the screen-verified submission used for native Claude and Codex panes. A default "known agent
  idle" verdict with no matching rule is not treated as evidence, and trust dialogs stop for a
  human. Claude and Codex only; slash commands (goals) are refused. Verified with real Claude and
  Codex in Herdr, in both editions: start, send, working/idle, wait, stop.
- **View.** In order: `/` made private; every source pinned by an `O_PATH` descriptor (nothing is
  staged on the host); a fresh tmpfs on `/tmp` (per launch, `tmp_size`); the slot's persistent
  private layer bound over `$HOME`, with each real top-level entry bound read-only (submounts
  included) on an empty placeholder of the same kind; `$HOME`'s aliases (other mount paths of the
  same directory) read-only, or tmpfs in `hidden` mode; writable binds (`home_shared` from the
  real tree, nested `home_private`, the slot, Git common directories, control directory, project
  `outputs`, `read_write`); masks (`home_hidden`: user-owned empty read-only tmpfs for a directory,
  `/dev/null` for a file); then, with `protect_system`, every other mount read-only. That last
  pass skips the layer only by its EXACT path, so the read-only binds under it are not skipped.
  Each remount uses the flags of the topmost mount at that path (a stacked mount's locked flags
  differ from those of the mount it covers).
- **Why a layer, not a read-only `$HOME`.** The first revision made the real `$HOME` read-only
  and set `CLAUDE_CONFIG_DIR` to move `~/.claude.json`. The site's Claude launcher ignores that
  variable and rewrites `~/.claude.json` through `~/.claude.json.tmp.<pid>` plus a rename, which
  needs a writable `$HOME` directory. The layer gives exactly that: new top-level files land in
  the per-slot layer, and `home_private_files` (default `.claude.json`) are seeded once as
  private copies. An overlay of the real `$HOME` was tried earlier and is impossible from a user
  namespace when `$HOME` has submounts; the kernel refuses to clone a mount with locked children.
- **Placeholders.** `prepare_state` (as the user, before any namespace) creates the placeholders,
  recreates symbolic links, seeds private files, and records what it created in
  `home-placeholders.json`. Placeholders and links for entries that are no longer bound are
  removed on the next launch, placeholders only while still empty.
- **Configuration.** `init` writes the full `sandbox` section with every default. `wrkslots
  sandbox show-config` prints the effective settings; `wrkslots sandbox write-defaults` adds
  missing keys. Unknown keys are refused, and keys renamed from the first draft are named in the
  refusal.

### agentctl

`agentctl start NAME --slot SLOT [--slot-isolation userns|cgroup|root] [--slot-project DIR]`, in
both editions (Rust: `SlotLaunch` in `StartOptions`). It asks `wrkslots shell-command --format
json` for the command, slot path, and effective isolation (root: see the relay adapter above). It
records the slot path
as the agent's cwd, runs the command in the new pane, and waits until the pane's shell PID is
again the sole foreground process, is a shell, and sits in a wrkslots slice. Then it starts the
harness normally. Verified in Herdr: a boxed pane under `userns` keeps the pane's shell PID and
sits in `wrkslots-<slot>.slice`.

### Builds

A build tool that materializes outputs on demand keeps only final outputs in the slot. Verified
with Buck2 inside an image-backed slot and the user-namespace sandbox: with `materializations =
deferred` and remote execution, a genrule producing a 50 MB intermediate and a 9-byte final
output left only the final output in the slot (`buck-out` 26 MB, mostly daemon state; the
intermediate never materialized). A second slot building the same targets reported 100% cache
hits and also did not materialize the intermediate. With local-only execution, the intermediate
is written inside the slot (74 MB `buck-out`). Any remote-execution API cache server outside the
slot works the same way; the sandbox leaves the network alone. `~/.buck` is a private
`home_private` directory, so each slot has its own daemon state.

### Configuration format (YAML)

`.wrkslots.yml` is literate YAML for new projects: `render_config` writes a header and a comment
above every key, and the comments come from tables kept beside the defaults
(`cli.CONFIG_KEY_DOCS`, `sandbox.SETTING_DOCS`/`LIMIT_DOCS`, `slotimage.IMAGE_DOCS`). The package
is standard-library only, so `yamlconfig.py` implements a strict, fail-closed subset reader that
refuses anything outside it with the line number. JSON files, from before this change, still load
through the same `_read_config_document`. Every rewrite keeps the file's format and saves the
previous bytes to `.wrkslots.yml.bak`. `config convert` switches formats. The refused constructs,
and why each one stays out, are listed in the module docstring.

### Review fixes (2026-09-28, independent review of #145 slot-images)

Threat model, per the owner: an accident boundary for cooperative agents (no stray checkouts or
files), not containment of a hostile agent.

- **Control directory (B1).** It is no longer bound writable. It is bound onto itself and
  remounted read-only (the whole tree, other slots' image mounts included) whatever
  `protect_system` says. `<control>/slot-state` and `<control>/slot-images` are masked, and the
  slot's own layer is bound over `$HOME` from a descriptor pinned beforehand. A boxed agent
  needs no control-directory writes: registry writers (`heartbeat`, `finish`, `write-handoff`)
  run outside the box. Heartbeats are diagnosis only, so a slot whose owner never heartbeats is
  merely reclaimed later.
- **IMAGE.json is untrusted (B2).** Its location must be the slot's canonical directory, a
  `.<slot>.*` sibling (removal and recovery fences), or the conversion staging directory, and its
  slot and type must match its directory. Kernel-backend operations go through `imagehelper.py`
  as root. The helper takes the user from SUDO_UID, opens every path once with O_NOFOLLOW, checks
  it through the descriptor, and acts on `/proc/self/fd/N`. The loop device is configured by
  ioctl from the pinned image descriptor (autoclear), so neither `mount` nor `losetup`
  re-resolves a path. Unmount re-checks the mount identity by lstat, then uses UMOUNT_NOFOLLOW
  (an open descriptor would keep the mount busy).
- **Root helper identity (B3, S1).** uid and gid come from SUDO_UID/SUDO_GID, and groups from
  getgrouplist for that user; a spec that disagrees is refused. The spec lives in
  `$XDG_RUNTIME_DIR/wrkslots` (tmpfs, 0700, read-only inside every box). It must be a 0600,
  singly linked regular file owned by the user, and it is deleted right after reading or on any
  failure. Stale specs a failed sudo left behind are removed by the next launch. Passing an
  inherited descriptor instead needs `sudo -C`, which sudoers must allow, so a file is used.
- **PID namespace for root mode (B4): tried, reverted.** The helper unshared mount and PID
  namespaces and forked a small init (fresh `/proc`, reaping, signal forwarding). Our own tests
  passed, including host PIDs hidden and signals forwarded. But the real claude launcher printed
  "Failed to connect to user scope bus via local transport: No data available" and hung: the
  systemd user bus cannot identify a peer in another PID namespace. So root mode shares the host
  PID namespace, and `/proc/<pid>/root` and ptrace of the user's other processes stay reachable
  (this host has no Yama ptrace restriction). Documented under "Limits of the box".
- **Fuse units (S5)** run in `<root_slice>-images.slice`, nested in the caller's slice, and are
  named by a SHA-256 of the image path.
- **Documented, not changed.** The user bus stays writable, so a `systemd-run --user` service
  starts outside the box (B5). `home_shared` directories hold hooks and settings that run unboxed
  (S4). Root mode allows setuid programs, `sudo` included, cannot set no_new_privs, and shares the
  host PID namespace (B4).

Follow-ups, not done:

- **S2**: a content command (status, finish, remove, convert) on an image slot whose image is not
  mounted should refuse outright, rather than warn and continue against the empty mount point.
- **S3**: `image convert` needs a journal so an interruption between copy and swap is recovered
  by `wrkslots recover`. Until then the guide says to convert only idle slots and to keep
  `--keep-original` for the first conversions.

### Coordinator box (project-box, 2026-09-29)

`wrkslots box` reuses the slot view with a `SlotView(coordinator=True)`: no slot, a writable scope
(`worktrees` or `project`), the private layer at `<control>/box-state/NAME/home`, and a list of
propagation roots (the control directory and the slot roots).

Mount propagation, verified on this host rather than assumed:

- A box built with `MS_REC|MS_SLAVE` on `/` receives host mounts. A bind of a slave source onto
  a non-shared target is a slave of the same master, so the control-directory bind inside the box
  receives image mounts made on the host after the box started. After the view is built, every
  mount except the one each root resolves through (and mounts below the roots) is made private.
  So a tmpfs mounted on the host in a sibling directory is not visible in the box. `findmnt -T`
  still lists it, because the copy lands on a shadowed slave mount; `stat` shows the directory's
  own device. Slot boxes stay `MS_REC|MS_PRIVATE`.
- A mount made from inside a box (root mode: sudo works; the helper's mount lands in the box's
  namespace) was invisible to the host. The image helper therefore runs in the host namespace
  whenever wrkslots detects a box (the spec directory is a mount point, or `WRKSLOTS_SANDBOX` is
  userns/root), as `systemd-run --user --wait --pipe --collect -- sudo -n python -I
  imagehelper.py ...`. The user manager lives in PID 1's mount namespace (checked: same `mnt:`
  inode as PID 1 and the Herdr server). The helper re-checks every path there; no new root code
  was needed, and userns boxes, where sudo cannot run, work too. Setns into the user manager's
  namespace from the helper was the alternative. It needs root-side PID pinning of the manager
  and re-opening every path after setns, and it would not help userns boxes.
- Verified: in root and userns boxes, `wrkslots create --representation image` mounts the slot
  in PID 1's namespace (`nsenter -t 1 -m findmnt`), the box sees it at once, and a slot created on
  the host after the box started appears inside and is writable.

Review fixes (2026-09-29):

- Only a coordinator box routes image operations to the host (it sets WRKSLOTS_BOX, and the
  image directory must be writable from it). A slot's box refuses them: its view is private,
  so a host-side mount was invisible to it, and the failed check left the mount and its loop
  device behind.
- Every image operation requested from a coordinator box is verified in the HOST mount table,
  read through the user manager (`systemd-run --user --pipe cat /proc/self/mountinfo`) and
  cached until the next host-side operation. The box's own table keeps copies of mounts that
  existed when it started, under parents the box made private. Host-side umount events never
  reach those copies, so they outlive the host mount. Observed: a state image unmounted on the
  host stayed listed in the box, and the old check refused halfway through `image unmount`.
  Removing or renaming a mount-point directory also runs on the host, because the kernel
  refuses both for a directory that is a mount point in the caller's own namespace. It detaches
  copies in other namespaces when the host does it, which also frees their loop devices. A mount
  that fails its check is undone.
- Those copies also keep the unmounted file system alive: a loop device stays bound and, for
  FUSE, fuse2fs never exits, so an unmount from a userns box waited forever on a slot that was
  mounted before the box started. After a host-side unmount, the empty mount-point directory is
  now replaced on the host by a fresh one (mkdir beside it, then rename over it). The kernel
  detaches every mount on a removed directory entry in every namespace, locked copies in user
  namespaces included. The same step runs on the host for FUSE when the servers do not exit
  within 10 s, which also covers copies held by unrelated namespaces (concurrent test harnesses).
  e2e_coordinator_images.sh checks that the box releases the loop devices and servers while it is
  still running.
- Detaching those copies inside the box when it is built is not possible in a userns box, where
  mounts inherited from the parent namespace are locked. Keeping their parents slaves would let
  every later host mount under those parents (sibling projects' slots, for example) appear in the
  box, so the host-side verification above was chosen.
- FUSE unmount scans /proc once for the image's fuse2fs servers (about 1.3 s on a 4,000-process
  host) and then waits on those PIDs by pidfd, instead of rescanning every 50 ms.

Follow-up, not done: the Python agentctl edition has no `--project-box`; the Rust edition does.
Add it if the cross-language differential ever needs the flag, or if Python-only hosts need
coordinator boxes.

Real runs: `accept_coordinator_box.sh` passes (phase 1 creates both kinds of slot; phase 2 is
agentctl-launched claude workers under root slot isolation, each confined to its slot). A Herdr
pane test of `agentctl start sub --harness claude --project-box --box-isolation root`: the boxed
claude ran the write probe (registry, image and worktree slots writable; primary checkout, a
sibling directory, and `~/work` read-only; the Herdr socket and the agentctl registry reachable)
and launched a codex worker with `agentctl start --slot wt1 --slot-isolation root` from inside its
box. The worker wrote only its slot. Codex's folder-trust question had to be answered once by a
human, and the first attempt with `--slot-isolation userns` failed as expected (the Meta codex
launcher's setuid step).

## Verification

- `py/wrkslots/tests/test_slot_images.py`: configuration defaults and migration rules, sandbox
  settings validation (including refusal of renamed keys), the full default section, merging
  defaults, path expansion, the view spec and the layer preparation against a fake `$HOME`, the
  child environment, slice nesting, owned-mount exclusion, and a real image lifecycle. Also real
  sandbox runs in both `userns` and `root` modes that probe every write rule, and a real `wrkslots
  run` on a plain-worktree slot created through the CLI (commit from the checkout allowed; primary
  checkout, outside paths, and real `$HOME` untouched).
- `py/wrkslots/tests/e2e_images.sh BACKEND LAYOUT`: init (image default), create, content, status,
  the same box probe on an image slot and on a plain-worktree slot, HOME modes, owner death,
  salvage-and-remove, convert to image and back, remove.
- `py/wrkslots/tests/accept_root_harness.sh`: opt-in; runs each harness named in
  `WRKSLOTS_ACCEPT_HARNESSES` headless through `wrkslots run --isolation root` with a write probe.
- Rust agentctl: `slot_*` and `start_slot_*` unit tests; Python agentctl: `test_agentctl_sessions.py
  -k slot`.

## Open questions

- Full clone per slot (copy-on-write template image with its own Git objects) instead of linked
  worktrees: stronger isolation of Git objects, but it changes the salvage and registration model.
- Host-wide early warning: read the file system's unallocated-space counters and freeze slot
  slices below a threshold.
- Root isolation leaves a short window after a verified submission in which the harness
  shows neither a working rule nor the interrupt hint; a `wait` issued in that window can
  return idle early.

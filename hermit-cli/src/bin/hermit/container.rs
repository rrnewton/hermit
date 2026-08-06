/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;

use hermit::Context;
use hermit::Error;
use hermit::SerializableError;
use nix::sched::CpuSet;
use nix::sched::sched_getaffinity;
use nix::unistd::Pid;
use reverie::process::Container;
use reverie::process::Mount;
use reverie::process::MountFlags;
use reverie::process::Namespace;

const GROUP_FILE: &str = "/etc/group";
const NSCD_DIR: &str = "/var/run/nscd";
const OVERFLOW_GID: &str = "65534";
const DEV_DIR: &str = "/dev";

/// Character devices every guest gets. Each is a pure kernel pseudo-device with
/// no host state behind it, plus `tty` for the controlling terminal. This is the
/// same shape the OCI runtime specification calls the default device set, minus
/// `/dev/console` (a host resource that a hermetic guest has no business
/// touching).
///
/// Anything not on this list — block devices, `/dev/kvm`, `/dev/mem`,
/// `/dev/net/tun`, GPU nodes, `/dev/kmsg` — stays out of the guest. See
/// [`minimal_dev_mounts`].
const DEFAULT_GUEST_DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// Symlinks staged inside the guest `/dev`. Every target resolves through the
/// guest's own deterministic `/proc` (mounted by [`default_container`]), so none
/// of them carries host state.
const GUEST_DEV_SYMLINKS: &[(&str, &str)] = &[
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
    ("ptmx", "pts/ptmx"),
];

// Bind mount sources must outlive Reverie's pre-exec container setup, which
// applies the mounts in the forked child before exec. Hold this guard in the
// caller until after `Container::run` returns so the backing temp files and
// directories still exist when the child binds them.
pub(super) struct MountGuard {
    _group_file: Option<tempfile::NamedTempFile>,
    _nscd_dir: Option<tempfile::TempDir>,
    _dev_dir: Option<tempfile::TempDir>,
}

impl MountGuard {
    /// A guard that owns no backing temp files, for container configurations
    /// (e.g. `--image`) that supply their filesystem from another source and do
    /// not use the frozen-identity bind mounts.
    pub(super) fn empty() -> Self {
        Self {
            _group_file: None,
            _nscd_dir: None,
            _dev_dir: None,
        }
    }
}

/// Snapshot the host group database into a private temp file, appending a
/// synthetic overflow group (`nobody:x:65534`) when the host lacks one. Binding
/// this frozen copy read-only over `/etc/group` keeps guest group-name
/// resolution stable across otherwise-identical runs.
fn frozen_group_file() -> Result<tempfile::NamedTempFile, Error> {
    let mut contents = fs::read_to_string(GROUP_FILE)
        .context("Failed to read the host group database for the guest")?;
    let has_overflow_group = contents.lines().any(|line| {
        line.split(':')
            .nth(2)
            .is_some_and(|gid| gid == OVERFLOW_GID)
    });
    if !has_overflow_group {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push_str("nobody:x:");
        contents.push_str(OVERFLOW_GID);
        contents.push_str(":\n");
    }

    let mut group_file = tempfile::NamedTempFile::new()
        .context("Failed to create the frozen group database for the guest")?;
    group_file
        .write_all(contents.as_bytes())
        .context("Failed to populate the frozen group database for the guest")?;
    group_file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))
        .context("Failed to set permissions on the frozen guest group database")?;
    Ok(group_file)
}

/// Deterministic identity-resolution mounts shared by `run`, `record`, and
/// `replay`: a frozen `/etc/group` and an empty directory over the host nscd
/// cache. These keep guest NSS lookups from reaching nondeterministic host
/// state (the nscd cache and the systemd-userdb socket), so record/replay
/// reproduce the same group/user resolution that `run` mode already enforces.
fn identity_mounts() -> Result<
    (
        Vec<Mount>,
        tempfile::NamedTempFile,
        Option<tempfile::TempDir>,
    ),
    Error,
> {
    let group_file = frozen_group_file()?;
    let mut mounts = vec![Mount::bind(group_file.path(), GROUP_FILE).readonly()];

    // Host nscd cache readiness is external state and can differ between runs.
    let nscd_dir = if Path::new(NSCD_DIR).is_dir() {
        let directory =
            tempfile::TempDir::new().context("Failed to create the empty guest nscd directory")?;
        mounts.push(Mount::bind(directory.path(), NSCD_DIR).readonly());
        Some(directory)
    } else {
        None
    };

    Ok((mounts, group_file, nscd_dir))
}

/// The names, in guest `/dev`, that [`minimal_dev_mounts`] will produce for
/// `extra_devices` plus the built-in default set. Used by tests and by callers
/// that need to describe the guest device surface without building it.
pub(super) fn guest_device_names(extra_devices: &[&str]) -> Vec<String> {
    DEFAULT_GUEST_DEVICES
        .iter()
        .chain(extra_devices.iter())
        .map(|name| (*name).to_owned())
        .collect()
}

/// Build a **scoped** guest `/dev` instead of inheriting the host's.
///
/// Without this, the guest's mount namespace inherits host `/dev` verbatim: every
/// block device, `/dev/kvm`, `/dev/mem`, GPU and network nodes, and whatever the
/// host happens to have left lying in the world-writable `/dev/shm`. That is both
/// an isolation hole and a nondeterminism source, since `/dev` contents differ
/// between hosts and between runs on the same host.
///
/// The tree is *staged on the host* in a private temp directory rather than built
/// directly at `/dev`, because Reverie applies mounts in order inside the forked
/// child: once anything is mounted over `/dev`, `/dev/null` no longer names the
/// host device node, so the allow-listed sources must be captured while host
/// `/dev` is still the one in view. Staging first and recursive-binding the
/// finished tree over `/dev` last avoids ever exposing host `/dev` under a second
/// path.
///
/// Contents:
/// * [`DEFAULT_GUEST_DEVICES`] plus `extra_devices`, bind-mounted from their host
///   nodes (a name absent on the host is skipped, not an error).
/// * A fresh, empty `tmpfs` at `/dev/shm`, so host POSIX shared-memory segments
///   are neither readable nor a cross-run channel.
/// * A private `devpts` instance at `/dev/pts`, so the guest cannot see host
///   pseudo-terminals.
/// * [`GUEST_DEV_SYMLINKS`], which resolve through the guest's own `/proc`.
///
/// Mount propagation cannot leak any of this back to the host: the container
/// unshares `CLONE_NEWNS` together with `CLONE_NEWUSER`, and the kernel converts
/// shared mounts to slaves when the new mount namespace is owned by a different
/// user namespace.
///
/// Returns the mounts plus the staging directory, which must outlive
/// `Container::run`.
pub(super) fn minimal_dev_mounts(
    extra_devices: &[&str],
) -> Result<(Vec<Mount>, tempfile::TempDir), Error> {
    let staging = tempfile::TempDir::new()
        .context("Failed to create the staging directory for the guest /dev")?;
    let root = staging.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o755))
        .context("Failed to set permissions on the staged guest /dev")?;

    let mut mounts = Vec::new();
    let mut staged: Vec<PathBuf> = Vec::new();
    for name in guest_device_names(extra_devices) {
        let host_node = Path::new(DEV_DIR).join(&name);
        // A host without the node (for example a `/dev/tty`-less batch context,
        // or `/dev/kvm` on a machine with no KVM) simply does not get it. This
        // only ever shrinks the guest's device surface, so it cannot widen the
        // hole this function exists to close.
        if !host_node.exists() {
            continue;
        }
        let target = root.join(&name);
        if staged.contains(&target) {
            continue;
        }
        // A bind mount needs its target inode to already exist. Stage a plain
        // empty file; the bind replaces it with the host character device.
        fs::File::create(&target)
            .with_context(|| format!("Failed to stage the guest {}", host_node.display()))?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o666))
            .with_context(|| format!("Failed to set permissions on the guest {name} node"))?;
        mounts.push(Mount::bind(&host_node, &target));
        staged.push(target);
    }

    // Fresh, empty, world-writable-with-sticky shared memory, matching the host
    // `/dev/shm` permissions a guest expects but sharing none of its contents.
    let shm = root.join("shm");
    fs::create_dir(&shm).context("Failed to stage the guest /dev/shm")?;
    fs::set_permissions(&shm, fs::Permissions::from_mode(0o1777))
        .context("Failed to set permissions on the guest /dev/shm")?;
    mounts.push(Mount::tmpfs(&shm).data("mode=1777"));

    // A private devpts instance. `ptmxmode` is what makes the `ptmx` symlink
    // below usable; no `gid=` option, because the tty group is not mapped into
    // the guest's user namespace.
    let pts = root.join("pts");
    fs::create_dir(&pts).context("Failed to stage the guest /dev/pts")?;
    mounts.push(Mount::devpts(&pts).data("ptmxmode=0666"));

    for (name, target) in GUEST_DEV_SYMLINKS {
        symlink(target, root.join(name))
            .with_context(|| format!("Failed to stage the guest /dev/{name} symlink"))?;
    }

    // Replace the guest's view of /dev with the staged tree. Recursive, so the
    // tmpfs and devpts mounts staged above come along. This must stay last: the
    // binds above resolve their sources against host /dev.
    mounts.push(Mount::bind(root, DEV_DIR).recursive());

    Ok((mounts, staging))
}

/// Deterministic hardening mounts shared by `run`, `record`, and `replay`:
/// [`identity_mounts`] plus a scoped guest `/dev` ([`minimal_dev_mounts`]).
/// `extra_devices` names additional entries of host `/dev` the caller has
/// established the guest genuinely needs — currently only `kvm`, for the KVM
/// backend, whose guest cannot start without it.
///
/// Returns the mounts plus a guard that must outlive container setup.
pub(super) fn hardening_mounts(extra_devices: &[&str]) -> Result<(Vec<Mount>, MountGuard), Error> {
    let (mut mounts, group_file, nscd_dir) = identity_mounts()?;
    let (dev_mounts, dev_dir) = minimal_dev_mounts(extra_devices)?;
    mounts.extend(dev_mounts);

    Ok((
        mounts,
        MountGuard {
            _group_file: Some(group_file),
            _nscd_dir: nscd_dir,
            _dev_dir: Some(dev_dir),
        },
    ))
}

/// [`hardening_mounts`] without the scoped `/dev`: the guest keeps the host's
/// device tree. Only for the explicit `run --host-dev` escape hatch.
pub(super) fn host_dev_hardening_mounts() -> Result<(Vec<Mount>, MountGuard), Error> {
    let (mounts, group_file, nscd_dir) = identity_mounts()?;
    Ok((
        mounts,
        MountGuard {
            _group_file: Some(group_file),
            _nscd_dir: nscd_dir,
            _dev_dir: None,
        },
    ))
}

fn choose_affinity_core(allowed: &[usize]) -> Option<usize> {
    (!allowed.is_empty()).then(|| allowed[rand::random_range(0..allowed.len())])
}

fn cpu_ids(affinity: &CpuSet) -> Vec<usize> {
    (0..CpuSet::count())
        .filter(|cpu| affinity.is_set(*cpu).unwrap_or(false))
        .collect()
}

pub(super) fn apply_affinity(container: &mut Container, pin_threads: bool) {
    if pin_threads {
        let affinity = sched_getaffinity(Pid::from_raw(0))
            .expect("failed to query the tracer's allowed CPU affinity mask");
        let allowed = cpu_ids(&affinity);
        let rand_core = choose_affinity_core(&allowed)
            .expect("the tracer's allowed CPU affinity mask is empty");
        tracing::info!("Pinning tracer and guest threads to core {}", rand_core);
        container.affinity(rand_core);
    }
}

pub fn default_container(pin_threads: bool) -> Container {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local")
        .mount(Mount::proc());

    apply_affinity(&mut container, pin_threads);
    container
}

/// PROTOTYPE: a container whose root filesystem is a materialized OCI image
/// rootfs. This is the *filesystem half* of hermit-as-container-runtime: the
/// guest's file inputs come deterministically from the pinned image rather than
/// from the host filesystem.
///
/// Like the replay chroot path (`replay.rs`), mounts are applied at their
/// literal (pre-chroot) target paths and then `chroot(rootfs)` makes them
/// visible under the new root. We therefore mount the deterministic `/proc`
/// *into* `<rootfs>/proc` (pre-created by the materializer) before chrooting, so
/// the guest sees a `/proc` after entering the image root. The image already
/// carries its own `/etc/group`, loader, and libc — pinned by the image digest —
/// so the frozen-identity hardening mounts that `run` normally adds are
/// unnecessary here; the returned [`MountGuard`] is empty.
///
/// This path does not get [`minimal_dev_mounts`]: the guest sees only whatever
/// `/dev` the image itself carries, which is already scoped rather than the
/// host's. That it is usually *empty* — no `/dev/null` — is a separate known
/// `--image` defect, tracked apart from the host-`/dev`-passthrough hole that
/// [`minimal_dev_mounts`] closes, and is not fixed here.
///
/// The CLI currently enables this only for the ptrace backend. Other backends
/// have distinct launch/runtime-file requirements and must be qualified before
/// they can safely share this filesystem setup.
pub(super) fn image_container(
    rootfs: &Path,
    tmpfs: &Path,
    pin_threads: bool,
) -> Result<(Container, MountGuard), Error> {
    let mut container = Container::new();
    container
        .unshare(Namespace::PID)
        .map_root()
        .hostname("hermetic-container.local")
        .domainname("local");

    // The cache is a deterministic input, not a writable container layer. Bind
    // it onto itself and remount it read-only so a guest cannot poison later
    // runs or mutate run one underneath `--verify` run two. A fresh writable
    // /tmp is mounted separately for ordinary scratch files.
    container.mount(Mount::bind(rootfs, rootfs));
    container.mount(
        Mount::new(rootfs)
            .flags(MountFlags::MS_BIND | MountFlags::MS_REMOUNT | MountFlags::MS_RDONLY),
    );
    container.mount(Mount::bind(tmpfs, rootfs.join("tmp")).rshared());

    // Mount the deterministic /proc into the target root. The materializer
    // guarantees <rootfs>/proc exists, so we do not need `touch_target()` (which
    // defers dir creation to the pre-exec child on a tiny clone stack).
    let proc_target = rootfs.join("proc");
    container.mount(Mount::proc().target(&proc_target));

    // Enter the image root last, mirroring the replay chroot ordering
    // (mounts first, then chroot).
    container.chroot(rootfs);

    apply_affinity(&mut container, pin_threads);
    Ok((container, MountGuard::empty()))
}

/// A [`default_container`] hardened with the deterministic mounts that `run`
/// mode applies: a frozen `/etc/group`, a hidden nscd cache, and a scoped guest
/// `/dev`. Record and replay use this so guest NSS resolution and device surface
/// match `run` and do not reach nondeterministic host state. The returned
/// [`MountGuard`] must be held until after `Container::run` returns.
///
/// Record and replay are ptrace-only, so they never need the KVM backend's
/// `/dev/kvm` and pass no extra devices.
pub(super) fn deterministic_container() -> Result<(Container, MountGuard), Error> {
    let mut container = default_container(true);
    let (mounts, mount_guard) = hardening_mounts(&[])?;
    container.mounts(mounts);
    Ok((container, mount_guard))
}

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    Ok(container
        .run(|| f().map_err(SerializableError::from))
        .context("Sandbox container exited unexpectedly")??)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The frozen group database must always resolve the overflow GID so that
    // guest group-name lookups (e.g. `groups`) do not depend on nondeterministic
    // host NSS. This is what keeps record/replay identity resolution matching
    // `run` mode regardless of whether the host `/etc/group` lists 65534.
    #[test]
    fn identity_hardening_freezes_group_with_overflow_entry() {
        let (mounts, guard) =
            hardening_mounts(&[]).expect("hardening mounts should be constructible");
        assert!(
            !mounts.is_empty(),
            "expected at least the frozen /etc/group mount"
        );
        let group_file = guard
            ._group_file
            .as_ref()
            .expect("identity hardening must produce a frozen group file");
        let contents = fs::read_to_string(group_file.path())
            .expect("frozen group database should be readable");
        assert!(
            contents
                .lines()
                .any(|line| line.split(':').nth(2) == Some(OVERFLOW_GID)),
            "frozen group database must resolve overflow gid {OVERFLOW_GID}:\n{contents}"
        );
    }

    /// Names actually present in the staged guest `/dev`, sorted.
    fn staged_entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .expect("staged guest /dev should be readable")
            .map(|entry| {
                entry
                    .expect("staged guest /dev entry should be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The staged tree must contain the intended nodes and *only* those. This is
    /// the positive half of the bracket: an allow-listed name that silently
    /// failed to stage would make the guest `/dev` unusable rather than merely
    /// over-broad, so assert presence, not just absence.
    #[test]
    fn staged_guest_dev_contains_exactly_the_minimal_set() {
        let (_mounts, staging) =
            minimal_dev_mounts(&[]).expect("guest /dev should be constructible");

        let mut expected: Vec<String> = DEFAULT_GUEST_DEVICES
            .iter()
            .filter(|name| Path::new(DEV_DIR).join(name).exists())
            .map(|name| (*name).to_owned())
            .collect();
        expected.push("shm".to_owned());
        expected.push("pts".to_owned());
        expected.extend(
            GUEST_DEV_SYMLINKS
                .iter()
                .map(|(name, _)| (*name).to_owned()),
        );
        expected.sort();

        assert!(
            expected.contains(&"null".to_owned()),
            "every supported host has /dev/null; a run without it is a staging bug"
        );
        assert_eq!(staged_entries(staging.path()), expected);
    }

    /// The negative half: no mount may expose a host `/dev` entry that is not on
    /// the allow list, and none may expose host `/dev` itself under a second
    /// path. Only the final recursive bind is allowed to target `/dev`.
    #[test]
    fn guest_dev_mounts_expose_no_unlisted_host_device() {
        let (mounts, staging) =
            minimal_dev_mounts(&[]).expect("guest /dev should be constructible");
        let root = staging.path();

        let (last, rest) = mounts.split_last().expect("at least the /dev bind");
        assert_eq!(last.get_target(), Path::new(DEV_DIR));
        assert_eq!(last.get_source(), Some(root));

        let allowed: Vec<PathBuf> = DEFAULT_GUEST_DEVICES
            .iter()
            .map(|name| Path::new(DEV_DIR).join(name))
            .collect();
        for mount in rest {
            assert!(
                mount.get_target().starts_with(root),
                "mount {:?} escapes the staging directory",
                mount.get_target()
            );
            if let Some(source) = mount.get_source()
                && source.starts_with(DEV_DIR)
            {
                assert!(
                    allowed.contains(&source.to_path_buf()),
                    "host device {} is not on the guest allow list",
                    source.display()
                );
            }
        }

        // Plant the check against a real host-only device rather than a
        // hypothetical one, when this host has one to plant against.
        for host_only in ["/dev/kvm", "/dev/mem", "/dev/kmsg"] {
            if Path::new(host_only).exists() {
                assert!(
                    !mounts
                        .iter()
                        .any(|mount| mount.get_source() == Some(Path::new(host_only))),
                    "{host_only} exists on this host and must not reach the guest by default"
                );
            }
        }
    }

    /// `extra_devices` is the only way a host device joins the guest set, and it
    /// must actually fire — an inert opt-in would silently break the KVM backend.
    #[test]
    fn extra_devices_are_added_only_when_requested_and_present() {
        let host_kvm = Path::new("/dev/kvm");
        let (with_kvm, _guard) =
            minimal_dev_mounts(&["kvm"]).expect("guest /dev should be constructible");
        let binds_kvm = with_kvm
            .iter()
            .any(|mount| mount.get_source() == Some(host_kvm));
        assert_eq!(
            binds_kvm,
            host_kvm.exists(),
            "requesting `kvm` must bind /dev/kvm exactly when the host has it"
        );

        assert_eq!(
            guest_device_names(&["kvm"]).last().map(String::as_str),
            Some("kvm")
        );
    }

    #[test]
    fn affinity_core_is_an_actual_member_of_a_sparse_allowed_mask() {
        let mut affinity = CpuSet::new();
        for cpu in [7, 31, 211] {
            affinity.set(cpu).expect("valid sparse CPU id");
        }
        let allowed = cpu_ids(&affinity);
        assert_eq!(allowed, [7, 31, 211]);
        for _ in 0..128 {
            let selected = choose_affinity_core(&allowed).expect("non-empty mask");
            assert!(allowed.contains(&selected), "selected CPU {selected}");
        }
        assert_eq!(choose_affinity_core(&[]), None);
    }
}

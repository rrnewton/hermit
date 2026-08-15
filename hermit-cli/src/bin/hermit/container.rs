/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::any::Any;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::panic;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

use anyhow::anyhow;
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
use reverie::process::RunError;

const GROUP_FILE: &str = "/etc/group";
const NSCD_DIR: &str = "/var/run/nscd";
const OVERFLOW_GID: &str = "65534";

// Bind mount sources must outlive Reverie's pre-exec container setup, which
// applies the mounts in the forked child before exec. Hold this guard in the
// caller until after `Container::run` returns so the backing temp files still
// exist when the child binds them.
pub(super) struct IdentityGuard {
    _group_file: Option<tempfile::NamedTempFile>,
    _nscd_dir: Option<tempfile::TempDir>,
}

impl IdentityGuard {
    /// A guard that owns no backing temp files, for container configurations
    /// (e.g. `--image`) that supply their filesystem from another source and do
    /// not use the frozen-identity bind mounts.
    pub(super) fn empty() -> Self {
        Self {
            _group_file: None,
            _nscd_dir: None,
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
/// Returns the mounts plus a guard that must outlive container setup.
pub(super) fn identity_hardening_mounts() -> Result<(Vec<Mount>, IdentityGuard), Error> {
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

    Ok((
        mounts,
        IdentityGuard {
            _group_file: Some(group_file),
            _nscd_dir: nscd_dir,
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
/// unnecessary here; the returned [`IdentityGuard`] is empty.
///
/// The CLI currently enables this only for the ptrace backend. Other backends
/// have distinct launch/runtime-file requirements and must be qualified before
/// they can safely share this filesystem setup.
pub(super) fn image_container(
    rootfs: &Path,
    tmpfs: &Path,
    pin_threads: bool,
) -> Result<(Container, IdentityGuard), Error> {
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

    // A minimal /dev. An OCI image layer ships no device nodes, so without this
    // the guest sees an empty /dev on a read-only root: `> /dev/null` fails with
    // EROFS and anything wanting a pty fails outright.
    //
    // Every node here is one whose contents a guest cannot use to observe the
    // host:
    //
    // * `null`, `zero` and `full` have kernel-defined, constant behaviour.
    // * `random` and `urandom` are bound only so that `open(2)` finds an inode.
    //   Detcore classifies these two by the path the guest opened
    //   (`FdType::Rng`, see `detcore/src/syscalls/files.rs`) and serves reads
    //   from its deterministic PRNG, so the host entropy pool is never read.
    //   Binding the host node therefore restores functionality without handing
    //   the guest host entropy -- but it does mean the determinism of these two
    //   rests on Detcore's path classification, not on the mount.
    //
    // Deliberately absent:
    //
    // * `/dev/tty` -- its contents *are* the caller's controlling terminal, so it
    //   is pure host coupling, and the guest's behaviour would depend on whether
    //   Hermit was invoked from a terminal or a pipe. Removing exactly that kind
    //   of environmental coupling is the point of image mode. Absent,
    //   `open("/dev/tty")` reports ENOENT instead of ENXIO; both mean "no
    //   controlling terminal".
    // * `/dev/shm` -- a writable shared-memory area is cross-process shared state
    //   and a determinism question in its own right, and nothing in the current
    //   image workloads needs it.
    let dev_root = rootfs.join("dev");
    for node in crate::image::DEV_BIND_TARGETS {
        container.mount(Mount::bind(
            Path::new("/dev").join(node),
            dev_root.join(node),
        ));
    }

    // A FRESH devpts instance, not a bind of the host's. `newinstance` numbers
    // this container's ptys from 0 independently of the host, which is both the
    // isolation-correct and the deterministic choice: binding the host
    // `/dev/ptmx` would allocate out of the host's devpts and leak host-global
    // pty numbers into the guest. `ptmxmode=0666` makes the instance's own
    // `pts/ptmx` usable, which is what the `/dev/ptmx -> pts/ptmx` symlink the
    // materializer creates points at.
    container.mount(
        Mount::devpts(dev_root.join("pts"))
            .data("newinstance,ptmxmode=0666")
            .flags(MountFlags::MS_NOSUID | MountFlags::MS_NOEXEC),
    );

    // Enter the image root last, mirroring the replay chroot ordering
    // (mounts first, then chroot).
    container.chroot(rootfs);

    apply_affinity(&mut container, pin_threads);
    Ok((container, IdentityGuard::empty()))
}

/// A [`default_container`] hardened with the deterministic identity mounts
/// (frozen `/etc/group`, hidden nscd cache) that `run` mode applies. Record and
/// replay use this so guest NSS resolution matches `run` and does not reach
/// nondeterministic host identity state. The returned [`IdentityGuard`] must be
/// held until after `Container::run` returns.
pub(super) fn deterministic_container() -> Result<(Container, IdentityGuard), Error> {
    let mut container = default_container(true);
    let (mounts, identity_guard) = identity_hardening_mounts()?;
    container.mounts(mounts);
    Ok((container, identity_guard))
}

/// Where the most recent panic inside the container child happened.
///
/// `catch_unwind` hands back the panic PAYLOAD but not the LOCATION, and the
/// location is the more useful half of a divergence report. A panic hook is the
/// only place the location is available, so record it there and read it back
/// after the unwind.
static PANIC_LOCATION: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn panic_location() -> &'static Mutex<Option<String>> {
    PANIC_LOCATION.get_or_init(|| Mutex::new(None))
}

/// Install the location-recording hook, once, in the container child.
///
/// The previous hook is still called, so the ordinary `thread '...' panicked
/// at ...` report continues to reach stderr; this only ADDS a machine-readable
/// copy of the location.
fn install_panic_location_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if let Some(location) = info.location() {
                let recorded = format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
                if let Ok(mut slot) = panic_location().lock() {
                    *slot = Some(recorded);
                }
            }
            previous(info);
        }));
    });
}

fn take_panic_location() -> String {
    panic_location()
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| "<unknown location>".to_string())
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<panic payload was not a string>".to_string()
    }
}

/// Run `f`, converting a panic into an ordinary error.
///
/// WHY THIS EXISTS. The container child's entry point is `extern "C"` (see
/// `reverie_process::clone::clone_with_stack`) and is therefore NOUNWIND. A
/// panic that reaches it hits `panic_cannot_unwind` and ABORTS, and the parent
/// reports that abort as `Signaled(SIGSEGV, true)` -- indistinguishable from a
/// genuine memory fault, with the cause carried nowhere except stderr. That has
/// already sent one investigation hunting a bad dereference that did not exist.
/// Catching the unwind HERE, in Hermit's own closure, means the panic never
/// reaches the nounwind frame and instead travels the result channel
/// `Container::run` already provides.
///
/// This deliberately does NOT change how a REAL fault behaves: `catch_unwind`
/// cannot catch `SIGSEGV`, so a genuine memory fault in the child still aborts
/// and is still reported as a signal. Keeping those two outcomes
/// distinguishable is the point of the change, so both directions are covered
/// by tests.
/// Deliberate fault injection for the two-directional test below. INERT unless
/// `HERMIT_TEST_CONTAINER_CHILD_FAULT` is set, so it cannot fire in normal use.
///
/// Both arms are required, and the SECOND is the one that gives the test its
/// force: if a real fault stopped reporting as a signal after this change, the
/// change would have made a genuine memory error indistinguishable from a
/// caught panic, which is the same blindness in the other direction.
fn inject_test_fault() {
    match std::env::var("HERMIT_TEST_CONTAINER_CHILD_FAULT")
        .ok()
        .as_deref()
    {
        Some("panic") => panic!("deliberate container-child panic for fault-injection testing"),
        Some("segv") => {
            // A genuine memory fault, NOT a panic. catch_unwind must not touch this.
            unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
        }
        _ => {}
    }
}

fn catch_child_panic<F, T>(f: &mut F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
{
    install_panic_location_hook();
    match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        inject_test_fault();
        f()
    })) {
        Ok(result) => result,
        Err(payload) => {
            let location = take_panic_location();
            Err(anyhow!(
                "panic in container child at {location}: {}",
                panic_message(&*payload)
            ))
        }
    }
}

/// Runs a container-child closure with panics converted to errors.
///
/// Every `Container::run` call site in Hermit should use this instead of
/// `run`, so that no Hermit closure can reach the nounwind child entry point
/// while unwinding. `with_container` uses it; so do the direct call sites in
/// `record_start`, which is the path the partial-revents-copyout replay
/// divergence takes.
pub trait RunGuarded {
    fn run_guarded<F, T>(&mut self, f: F) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned;
}

impl RunGuarded for Container {
    fn run_guarded<F, T>(&mut self, mut f: F) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.run(move || {
            install_panic_location_hook();
            match panic::catch_unwind(panic::AssertUnwindSafe(|| {
                inject_test_fault();
                f()
            })) {
                Ok(result) => result,
                Err(payload) => Err(SerializableError::from(anyhow!(
                    "panic in container child at {}: {}",
                    take_panic_location(),
                    panic_message(&*payload)
                ))),
            }
        })
    }
}

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    Ok(container
        .run(|| catch_child_panic(&mut f).map_err(SerializableError::from))
        .context("Sandbox container exited unexpectedly")??)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_panic_becomes_an_error_naming_message_and_location() {
        let mut f = || -> Result<(), Error> { panic!("divergence detail that must survive") };
        let error = catch_child_panic(&mut f).expect_err("a panic must not be reported as success");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("divergence detail that must survive"),
            "panic message was lost: {rendered}"
        );
        assert!(
            rendered.contains("container.rs:"),
            "panic location was lost, which is the half catch_unwind does not give us: {rendered}"
        );
    }

    #[test]
    fn ordinary_results_pass_through_unchanged() {
        let mut ok = || -> Result<u32, Error> { Ok(7) };
        assert_eq!(catch_child_panic(&mut ok).unwrap(), 7);
        let mut err = || -> Result<u32, Error> { Err(anyhow!("a plain error, not a panic")) };
        let rendered = format!("{:#}", catch_child_panic(&mut err).unwrap_err());
        assert!(
            rendered.contains("a plain error, not a panic"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("panic in container child"),
            "an ordinary error must not be relabelled as a panic: {rendered}"
        );
    }

    // The frozen group database must always resolve the overflow GID so that
    // guest group-name lookups (e.g. `groups`) do not depend on nondeterministic
    // host NSS. This is what keeps record/replay identity resolution matching
    // `run` mode regardless of whether the host `/etc/group` lists 65534.
    #[test]
    fn identity_hardening_freezes_group_with_overflow_entry() {
        let (mounts, guard) =
            identity_hardening_mounts().expect("identity hardening mounts should be constructible");
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

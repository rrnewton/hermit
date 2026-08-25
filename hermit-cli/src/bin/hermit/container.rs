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
use hermit::FailureKind;
use hermit::SerializableError;
use nix::sched::CpuSet;
use nix::sched::sched_getaffinity;
use nix::sys::signal::SaFlags;
use nix::sys::signal::SigAction;
use nix::sys::signal::SigHandler;
use nix::sys::signal::SigSet;
use nix::sys::signal::Signal;
use nix::sys::signal::sigaction;
use nix::unistd::Pid;
use reverie::process::Container;
use reverie::process::ExitStatus;
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

/// Tie the container's init process to the lifetime of the `hermit` process
/// that created it.
///
/// [`Container::run`] forks the process that carries the guest into a fresh PID
/// namespace, so that process is PID 1 *of that namespace*. Linux gives a
/// namespace init the same protection it gives the system init: a signal whose
/// disposition is `SIG_DFL` is discarded rather than delivered, even when the
/// sender is in an ancestor namespace. Only `SIGKILL` and `SIGSTOP` from an
/// ancestor still get through.
///
/// That is why an external deadline could not end a hung run. `timeout N`
/// sends `SIGTERM` to the process group; the outer `hermit` process obeys it
/// and dies, the init process discards it, `timeout` sees its own direct child
/// gone and exits 124 without ever escalating, and the init process is
/// reparented to host PID 1 and keeps running. `timeout --kill-after` does not
/// help, because the escalation is conditioned on that already-dead direct
/// child. Three runs survived this way for more than 45 hours and filled the
/// filesystem.
///
/// `PR_SET_PDEATHSIG` closes that hole from inside: when the thread that forked
/// this process exits, the kernel sends us `SIGKILL`, which is precisely one of
/// the two signals a namespace init cannot ignore. Every existing caller keeps
/// working and no exit code changes, which is why this lives here rather than
/// in each of the ~20 scripts that wrap `hermit` in `timeout`.
///
/// Reverie uses the same idiom for exec'd untraced members in
/// `safeptrace/src/notifier.rs`.
///
/// One residual gap: if the parent somehow died between the fork and this call,
/// no death signal will ever arrive. The usual `getppid()` guard for that race
/// is unavailable here, because inside a new PID namespace the out-of-namespace
/// parent is not mapped and `getppid()` reports 0 whether or not it is alive.
/// Closing it completely would mean arming the signal inside
/// `Container::run` before container setup, which is a Reverie-side change.
/// The window is a few milliseconds of container setup, and the case this
/// guards against is a supervisor deadline seconds or minutes later.
fn arm_parent_death_signal() -> Result<(), Error> {
    // SAFETY: `PR_SET_PDEATHSIG` only sets the calling thread's parent-death
    // signal attribute. It reads and writes no caller memory, and the remaining
    // arguments are ignored for this option.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == -1 {
        return Err(std::io::Error::last_os_error()).context(
            "Failed to arm the container init parent-death signal \
             (prctl(PR_SET_PDEATHSIG, SIGKILL)); refusing to start a guest that \
             an expiring deadline could not then kill",
        );
    }
    Ok(())
}

/// The signals whose default action is to terminate the process and which a
/// supervisor, a shell, or a terminal actually sends when it wants a run to
/// stop. Every one of them is discarded by the kernel while the container init
/// leaves it at `SIG_DFL`, so all three are dead letters today: `SIGTERM` from
/// a deadline, `SIGINT` from Ctrl-C, and `SIGHUP` when the owning terminal or
/// agent session goes away.
const CONTAINER_INIT_STOP_SIGNALS: [Signal; 3] = [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP];

/// Terminate the container init on a stop signal.
///
/// The exit code is the conventional `128 + signo` a shell reports for a
/// process killed by that signal, rather than a genuine signalled wait status,
/// because a namespace init cannot produce the latter. The usual way to honour
/// a signal faithfully -- restore `SIG_DFL`, unblock, re-raise -- is measurably
/// unavailable here: a plain process dies from that sequence with
/// `WIFSIGNALED` set, but a namespace init survives it, because a self-sent
/// signal does not come from an ancestor namespace and so is discarded exactly
/// like the original. Reporting `128 + signo` is the closest honest
/// approximation available.
///
/// Exiting is a complete teardown, not just this process leaving: the kernel
/// `SIGKILL`s every remaining member of a PID namespace whose init exits, so
/// the guest goes with it instead of being orphaned into a namespace with no
/// init.
extern "C" fn on_container_init_stop_signal(signal: libc::c_int) {
    // SAFETY: `_exit` is async-signal-safe and is the only thing this handler
    // does. It deliberately skips destructors: there is nothing in this process
    // whose cleanup is worth blocking a requested shutdown on, and the
    // caller-side guards live in the parent process.
    unsafe { libc::_exit(128 + signal) }
}

/// Make the container init answer the signals a supervisor actually sends.
///
/// [`arm_parent_death_signal`] handles the case where the `hermit` process
/// dies. This handles the case where it does not: a `SIGTERM` aimed at the run
/// itself, a Ctrl-C, or a `SIGHUP` when the session ends. The kernel discards
/// those only while the disposition is `SIG_DFL`; installing any handler at all
/// is what makes them deliverable to a namespace init, so this both restores
/// the expected behaviour and gives hermit a place to shut a run down on
/// purpose rather than being shot.
///
/// The inherited signal mask matters as much as the disposition. A blocked
/// signal stays pending forever and the handler never runs, which would leave
/// the same silent no-op with a handler installed to suggest otherwise, so the
/// mask is cleared for these three. `record_start.rs` learned the same lesson
/// about `SIGALRM`.
fn install_container_init_stop_handlers() -> Result<(), Error> {
    let action = SigAction::new(
        SigHandler::Handler(on_container_init_stop_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );

    let mut unblock = SigSet::empty();
    for signal in CONTAINER_INIT_STOP_SIGNALS {
        // SAFETY: the handler performs only `_exit`, which is
        // async-signal-safe, and the action outlives the call.
        unsafe { sigaction(signal, &action) }
            .with_context(|| format!("Failed to install the container init {signal} handler"))?;
        unblock.add(signal);
    }

    unblock
        .thread_unblock()
        .context("Failed to unblock the container init stop signals")?;

    Ok(())
}

/// Arm both container-init guards. Call this as the FIRST statement inside any
/// `Container::run` closure that is not going through [`with_container`].
///
/// Why this is public rather than folded into [`with_container`]: `hermit record`
/// -- every spelling, including `record --verify` -- calls [`Container::run`]
/// directly at six sites in `record_start.rs` and never goes through
/// `with_container`. Those containers come from `recording_container()` ->
/// `deterministic_container()` -> `default_container(true)`, which unshares
/// `Namespace::PID`, so each one is a namespace init with exactly the bug the
/// guards exist to fix. An adversarial review of the original change caught this:
/// the claim that "all entry points funnel through `with_container`" was FALSE,
/// and `record --verify` is precisely the long-running command an external
/// deadline most needs to be able to end.
///
/// The structurally better home for this is reverie's `Container::run`, before
/// `setup()`, which would also close the fork-to-prctl window. That is a
/// reverie-side change plus a pin bump; this closes the six sites now.
pub(super) fn arm_container_init_guards() -> Result<(), SerializableError> {
    arm_parent_death_signal().map_err(SerializableError::from)?;
    install_container_init_stop_handlers().map_err(SerializableError::from)?;
    Ok(())
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
/// ⚠️ AND IT TAKES THE CALL SITE, BECAUSE A FAULT THAT CANNOT BE AIMED CANNOT
/// TEST MORE THAN THE FIRST SITE IT REACHES. `record` enters a container at six
/// sites; with only `HERMIT_TEST_CONTAINER_CHILD_FAULT` set, the FIRST child to
/// run faults and every later stage is never entered. Two of the six -- the
/// replay stages of `--verify` and `--verify-with-gdbex` -- are therefore
/// unreachable by any test, so their classification was asserted rather than
/// measured. `HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE` names which one to hit.
///
/// A LABEL RATHER THAN AN OCCURRENCE INDEX, deliberately. An index is positional:
/// it silently retargets the moment a call site is added, removed or reordered,
/// and the test keeps passing while aiming somewhere else. A label is identity,
/// and identity is the thing whose absence made these two sites untestable. An
/// occurrence counter also cannot be process-local -- each `run_guarded` forks a
/// fresh child and this runs in the CHILD, so a static counter resets to zero
/// every time.
///
/// Unset `..._SITE` keeps the previous behaviour exactly: fault at whichever site
/// is reached first. Existing callers and tests are unaffected.
fn inject_test_fault(site: &str) {
    if std::env::var("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE").is_ok_and(|want| want != site) {
        return;
    }
    match std::env::var("HERMIT_TEST_CONTAINER_CHILD_FAULT")
        .ok()
        .as_deref()
    {
        Some("panic") => {
            panic!("deliberate container-child panic for fault-injection testing at site {site}")
        }
        Some("segv") => {
            // A genuine memory fault, NOT a panic. catch_unwind must not touch this.
            unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
        }
        _ => {}
    }
}

/// Returns a [`SerializableError`] rather than a bare [`Error`] so the CLASS
/// survives: a caught panic is tagged HERE, at the only point that still knows
/// one happened, and the tag then crosses the process boundary with the message.
fn catch_child_panic<F, T>(f: &mut F) -> Result<T, SerializableError>
where
    F: FnMut() -> Result<T, Error>,
{
    install_panic_location_hook();
    match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        inject_test_fault("with_container");
        f()
    })) {
        Ok(result) => result.map_err(SerializableError::from),
        Err(payload) => {
            let location = take_panic_location();
            Err(SerializableError::from(anyhow!(
                "panic in container child at {location}: {}",
                panic_message(&*payload)
            ))
            .into_panic())
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
    /// Runs a container-child closure with panics converted to errors, carrying
    /// the CALL SITE'S IDENTITY into the child.
    ///
    /// ⚠️ THE LABEL IS NOT OPTIONAL, AND THAT IS DELIBERATE. An unlabelled form was
    /// tried and removed: every site would have compiled while silently opting out
    /// of being addressable, which is exactly the state that left two sites
    /// untestable. Requiring the argument makes a new call site declare what it is,
    /// and `cargo` asks the question at the moment the site is added.
    ///
    /// The label is inert in production. It is read only by `inject_test_fault`,
    /// which is itself inert unless the test environment variables are set.
    fn run_guarded_at<F, T>(
        &mut self,
        site: &'static str,
        f: F,
    ) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned;
}

impl RunGuarded for Container {
    fn run_guarded_at<F, T>(
        &mut self,
        site: &'static str,
        mut f: F,
    ) -> Result<Result<T, SerializableError>, RunError>
    where
        F: FnMut() -> Result<T, SerializableError>,
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.run(move || {
            install_panic_location_hook();
            match panic::catch_unwind(panic::AssertUnwindSafe(|| {
                inject_test_fault(site);
                f()
            })) {
                Ok(result) => result,
                // Tagged AT THE CATCH SITE, the only place that still knows a
                // panic is what happened. Everything downstream sees prose.
                Err(payload) => Err(SerializableError::from(anyhow!(
                    "panic in container child at {}: {}",
                    take_panic_location(),
                    panic_message(&*payload)
                ))
                .into_panic()),
            }
        })
    }
}

/// The container child exited with a status IT DID NOT CHOOSE.
///
/// ⚠️ THIS TYPE EXISTS BECAUSE THE STATUS USED TO BE THROWN AWAY HERE, and the
/// information was never missing — only discarded. reverie reports the child's
/// real status as a typed `RunError::ExitStatus(ExitStatus)`; `with_container`
/// then wrote `.context("Sandbox container exited unexpectedly")?`, which turns
/// that typed value into an opaque `anyhow::Error` whose exit code survives only
/// inside Display prose. Nothing downstream could branch on it without parsing
/// English, so a tracer panic (container child dies with Rust's 101) and an
/// ordinary CLI error — a bad flag, an unwritable log path — became the same
/// thing one layer up.
///
/// Carrying it as a downcastable type is the whole fix: no new plumbing, no new
/// channel, just stopping the discard. `main` recovers it with
/// `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct ContainerChildExit(pub ExitStatus);

impl std::fmt::Display for ContainerChildExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep the human text the caller already saw. This type changes what is
        // MACHINE-readable, not what an operator reads.
        write!(f, "Sandbox container exited unexpectedly: {:?}", self.0)
    }
}

impl std::error::Error for ContainerChildExit {}

/// The container child PANICKED, and the panic was caught and reported.
///
/// Distinct from [`ContainerChildExit`], which is the child dying of something
/// no handler caught. Both are "the tracer broke"; neither is an ordinary CLI
/// error, and before this they were all three the same thing.
#[derive(Debug)]
pub struct ContainerChildPanic(pub Error);

impl std::fmt::Display for ContainerChildPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ContainerChildPanic {}

/// Helper to run a function inside a container, taking care to display any
/// errors and propagate the exit status.
pub fn with_container<F, T>(container: &mut Container, mut f: F) -> Result<T, Error>
where
    F: FnMut() -> Result<T, Error>,
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    classify_container_result(container.run(|| {
        // Runs in the freshly forked container init, not in the caller.
        arm_container_init_guards()?;
        catch_child_panic(&mut f)
    }))
}

/// Turn a `Container::run` / [`RunGuarded::run_guarded`] outcome into an error
/// whose CLASS is still readable by `classify_failure`.
///
/// ⚠️ SHARED BECAUSE `with_container` IS NOT THE ONLY BOUNDARY. `hermit record`
/// -- every spelling -- calls [`RunGuarded::run_guarded`] directly at six sites
/// in `record_start.rs`, so a discard there loses exactly what this change
/// exists to preserve and the failure surfaces as `class=cli-error`.
pub fn classify_container_result<T>(
    ran: Result<Result<T, SerializableError>, RunError>,
) -> Result<T, Error> {
    match ran {
        // The child ran and REPORTED something. Whether that was a caught panic
        // or an error it chose to return is exactly what `kind` preserves; both
        // used to arrive as indistinguishable prose.
        Ok(Ok(value)) => Ok(value),
        Ok(Err(reported)) => {
            let panicked = reported.kind() == FailureKind::Panic;
            let error = Error::from(reported);
            Err(if panicked {
                Error::new(ContainerChildPanic(error))
            } else {
                error
            })
        }
        // PRESERVED, not flattened: the child died with a status it did not pick.
        Err(RunError::ExitStatus(status)) => Err(Error::new(ContainerChildExit(status))),
        // A spawn failure is a genuine CLI-side failure and keeps its prose.
        Err(error @ RunError::Spawn(_)) => {
            Err(Error::new(error).context("Sandbox container failed to spawn"))
        }
    }
}

/// Postfix spelling of [`classify_container_result`], so a direct
/// `run_guarded` call site reads the same way `with_container` does.
pub trait Classified<T> {
    fn classified(self) -> Result<T, Error>;
}

impl<T> Classified<T> for Result<Result<T, SerializableError>, RunError> {
    fn classified(self) -> Result<T, Error> {
        classify_container_result(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_panic_becomes_an_error_naming_message_and_location() {
        let mut f = || -> Result<(), Error> { panic!("divergence detail that must survive") };
        let reported =
            catch_child_panic(&mut f).expect_err("a panic must not be reported as success");
        // The discriminant is the point: prose alone could not say "panic".
        assert_eq!(
            reported.kind(),
            FailureKind::Panic,
            "a caught panic must be tagged as one at the catch site"
        );
        let error = Error::from(reported);
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
        let reported = catch_child_panic(&mut err).unwrap_err();
        assert_eq!(
            reported.kind(),
            FailureKind::Error,
            "an ordinary reported error must NOT be tagged as a panic"
        );
        let rendered = format!("{:#}", Error::from(reported));
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

//! Shared launch mechanics used by the opt-in route and unchanged physical controls.
//! No source qualification, Ready, policy implementation or second reaper.
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::time::Duration;

use reverie_inguest::launcher::exec_stop::ChildOwner;
use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::InitialExecStop;
use reverie_inguest::launcher::exec_stop::LaunchFailure;
use reverie_inguest::launcher::exec_stop::physical::CancelledPreparation;
use reverie_inguest::launcher::exec_stop::physical::Preparation;
use reverie_inguest::launcher::exec_stop::physical::PreparationFailure;
use reverie_inguest::launcher::exec_stop::physical::PreparedMapping;
use reverie_inguest::launcher::exec_stop::producer::EntryPoint;
use reverie_inguest::launcher::exec_stop::producer::RuntimeImageFacts;
use reverie_rpc_transport::RpcServer;

use super::MappedFirstAdmission;
pub(crate) const WAIT: Duration = Duration::from_secs(10);
pub(crate) fn need(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.to_owned()))
    }
}
pub(crate) fn rpc(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
/// A packet-lease refusal from the producer, as the error Hermit's lease
/// always returned: an `io::Error` the producer passed through (the
/// admission's watched-peer refusal, a descriptor call) comes back unchanged,
/// and any other refusal is `InvalidData` with the producer's exact text.
pub(crate) fn producer_error(error: Box<dyn std::error::Error>) -> io::Error {
    match error.downcast::<io::Error>() {
        Ok(error) => *error,
        Err(error) => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
    }
}
/// The signals Detcore's subscriptions reserve: SIGSYS always, SIGSEGV when
/// Detcore intercepts CPUID or RDTSC. The ToolInitialization observation and
/// the producer's arming both use this one computation.
pub(crate) fn reserved_signals(config: &detcore::Config) -> u64 {
    // Token for token the expression init_trial.rs used inline.
    use reverie::Tool;
    let subscriptions = <detcore::Detcore as Tool>::subscriptions(config);
    let mut reserved_signals = 1_u64 << (libc::SIGSYS - 1);
    if subscriptions.has_cpuid() || subscriptions.has_rdtsc() {
        reserved_signals |= 1_u64 << (libc::SIGSEGV - 1);
    }
    reserved_signals
}
pub(crate) async fn bounded<T>(
    future: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(WAIT, future).await.map_err(rpc)?
}
pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

// H owns no kernel wait mechanism: each slot retains the actual existing R
// capability. Place this value outside ALL block_on/catch_unwind expressions.
pub(crate) struct RetainedPhysical<'fd> {
    pub(crate) initial: Option<InitialExecStop<'fd>>,
    pub(crate) mapped: Option<PreparedMapping<'fd>>,
    pub(crate) preparation_failure: Option<Box<PreparationFailure<'fd>>>,
    pub(crate) unconfirmed_initial: Option<Box<ChildOwner<'fd>>>,
    pub(crate) launch_failure: Option<LaunchFailure<'fd>>,
    pub(crate) receipt: CleanupReceipt,
    pub(crate) cancelled: Option<CancelledPreparation>,
}
impl<'fd> RetainedPhysical<'fd> {
    // Construct this BEFORE spawn; store either returned capability immediately.
    pub(crate) fn empty(receipt: CleanupReceipt) -> Self {
        Self {
            initial: None,
            mapped: None,
            preparation_failure: None,
            unconfirmed_initial: None,
            launch_failure: None,
            receipt,
            cancelled: None,
        }
    }
    pub(crate) fn retain_spawn(
        &mut self,
        result: std::result::Result<InitialExecStop<'fd>, LaunchFailure<'fd>>,
    ) {
        match result {
            Ok(initial) => self.initial = Some(initial),
            Err(failure) => self.launch_failure = Some(failure),
        }
    }
    pub(crate) fn has_cleanup_capability(&self) -> bool {
        self.initial.is_some()
            || self.mapped.is_some()
            || self.preparation_failure.is_some()
            || self.unconfirmed_initial.is_some()
            || self.launch_failure.is_some()
    }
    pub(crate) fn initial(&self) -> io::Result<&InitialExecStop<'fd>> {
        self.initial
            .as_ref()
            .ok_or_else(|| match self.launch_failure.as_ref() {
                Some(failure) => io::Error::other(format!("actual launch refused: {failure:?}")),
                None => invalid("actual initial stop absent"),
            })
    }
    pub(crate) fn mapped(&self) -> io::Result<&PreparedMapping<'fd>> {
        self.mapped
            .as_ref()
            .ok_or_else(|| invalid("actual prepared mapping absent"))
    }
    /// Safety: exact existing Preparation contract, including original-thread
    /// affinity, inherited leases, real context/PKRU and external containment.
    /// Image execution qualification is NOT asserted by this map-only adapter.
    pub(crate) unsafe fn prepare(&mut self, inputs: Preparation<'fd>) -> io::Result<()> {
        let initial = self
            .initial
            .take()
            .ok_or_else(|| invalid("actual initial stop absent"))?;
        // No await or fallible diagnostic between consuming and retaining owner.
        match unsafe { initial.prepare_mapping(inputs) } {
            Ok(mapped) => {
                self.mapped = Some(mapped);
                Ok(())
            }
            Err(failure) => {
                self.preparation_failure = Some(failure);
                match self.preparation_failure.as_ref() {
                    Some(retained) => Err(io::Error::other(format!(
                        "actual mapping refused: {}",
                        retained.error
                    ))),
                    None => Err(invalid("retained preparation failure absent")),
                }
            }
        }
    }
    pub(crate) fn retire_once(&mut self) {
        if let Some(initial) = self.initial.take()
            && let Err(owner) = initial.cancel()
        {
            self.unconfirmed_initial = Some(owner);
        }
        if let Some(mapped) = self.mapped.take() {
            match mapped.cancel() {
                Ok(cancelled) => self.cancelled = Some(cancelled),
                Err(mapped) => self.mapped = Some(*mapped),
            }
        }
        if let Some(failure) = self.preparation_failure.as_mut() {
            let _ = failure.retry_cleanup();
        }
        if let Some(failure) = self.launch_failure.as_mut() {
            let _ = failure.retry_cleanup();
        }
        if let Some(owner) = self.unconfirmed_initial.take()
            && let Err(owner) = (*owner).cancel()
        {
            self.unconfirmed_initial = Some(owner);
        }
    }
}
// No Drop success receipt, new reaper, raw task escape, wait loop, entry API,
// generic proof constructor or test suppression is added. The enclosing root
// MUST retain this component and all its leases until actual confirmed cleanup.

/// Exact data coordinates supplied from independently retained output, then
/// audited against the SAME full image bytes by the producer
/// (`AuditedRuntimeImage::new`). These are not source proof.
pub(crate) struct ObservedImage {
    pub(crate) image_sha256: [u8; 32],
    pub(crate) layout_address: u64,
    pub(crate) reference_address: u64,
    pub(crate) handler_address: u64,
    pub(crate) handler_bytes: usize,
    pub(crate) handler_sha256: [u8; 32],
    pub(crate) dev_null_address: u64,
    pub(crate) sites: [[u64; 4]; 4],
    pub(crate) ordinary_post_ips: Vec<u64>,
}
impl ObservedImage {
    /// The producer's facts for this image: these observations, one entry
    /// for the initial birth, no successor, and no data ranges (x86-64).
    pub(crate) fn facts(&self, initial_entry: EntryPoint) -> RuntimeImageFacts {
        RuntimeImageFacts {
            image_sha256: self.image_sha256,
            layout_address: self.layout_address,
            reference_address: self.reference_address,
            sites: self.sites,
            ordinary_post_ips: self.ordinary_post_ips.clone(),
            handler_address: self.handler_address,
            handler_bytes: self.handler_bytes as u64,
            handler_sha256: self.handler_sha256,
            dev_null_address: self.dev_null_address,
            initial_entry: Some(initial_entry),
            successor_entry: None,
            data_ranges: Vec::new(),
        }
    }
}
// A caught payload may have a panicking destructor. Retain the first payload
// (including an earlier setup/observation panic); never drop a secondary payload
// while actual child retirement is unconfirmed. This exceptional leak grants no
// cleanup credit. The containing fixture remains externally bounded.
pub(crate) type PanicPayload = Box<dyn std::any::Any + Send + 'static>;
pub(crate) fn retain_unwind<T>(
    first: &mut Option<PanicPayload>,
    operation: impl FnOnce() -> T,
) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Some(value),
        Err(payload) => {
            if first.is_none() {
                *first = Some(payload);
            } else {
                std::mem::forget(payload);
            }
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetirementStep {
    Complete,
    RetryRetained,
    RetainFrame,
}

// This is host-side ownership control, not a terminal receipt constructor.
// The physical caller supplies the existing receipt snapshot and capability
// presence AFTER the caught attempt. Generic parameters permit data-only unwind
// tests to exercise this exact transition without manufacturing a ChildOwner.
pub(crate) fn guarded_retirement<T>(
    owner: &mut T,
    first: &mut Option<PanicPayload>,
    retire: impl FnOnce(&mut T),
    snapshot: impl FnOnce(&T) -> (bool, bool, bool),
) -> RetirementStep {
    let _ = retain_unwind(first, || retire(owner));
    let Some((created, confirmed, retained)) = retain_unwind(first, || snapshot(owner)) else {
        // An unreadable receipt provides no permission to release the frame.
        return RetirementStep::RetainFrame;
    };
    if !created || confirmed {
        RetirementStep::Complete
    } else if retained {
        RetirementStep::RetryRetained
    } else {
        RetirementStep::RetainFrame
    }
}

pub(crate) fn retain_unconfirmed_frame() -> ! {
    // Failure-only last resort: no diagnostics, TLS-dependent park, allocation,
    // callback, return, panic or resource destruction. Keep every outer backing,
    // descriptor/admission owner and caught payload live until the existing
    // whole-subtree containment terminates this FAILED/INCOMPLETE fixture. CPU
    // spin is deliberately limited to this lost-capability path, not retries.
    loop {
        std::hint::spin_loop();
    }
}

pub(crate) fn cleanup_all(
    a: &mut RetainedPhysical<'_>,
    b: &mut RetainedPhysical<'_>,
    first: &mut Option<PanicPayload>,
) {
    // No second wait/reaper, future, detach, entry or timeout release. Existing
    // R cancellation does its bounded original-owner wait. External containment
    // bounds this entire synchronous root if actual retirement never confirms.
    loop {
        let sa = guarded_retirement(
            a,
            first,
            |root| root.retire_once(),
            |root| {
                let state = root.receipt.snapshot();
                (
                    state.child_created,
                    state.confirmed,
                    root.has_cleanup_capability(),
                )
            },
        );
        let sb = guarded_retirement(
            b,
            first,
            |root| root.retire_once(),
            |root| {
                let state = root.receipt.snapshot();
                (
                    state.child_created,
                    state.confirmed,
                    root.has_cleanup_capability(),
                )
            },
        );
        if sa == RetirementStep::Complete && sb == RetirementStep::Complete {
            return;
        }
        if sa == RetirementStep::RetainFrame || sb == RetirementStep::RetainFrame {
            retain_unconfirmed_frame();
        }
        // Only actual retained capabilities are retried by retire_once. A lost
        // capability is never reconstructed from a PID, receipt or kill request.
    }
}

/// This fixture only invokes the real server's existing cooperative serve_until;
/// no replacement coordinator or GlobalTool is defined.
pub(crate) struct Serving {
    pub(crate) task: Option<tokio::task::JoinHandle<Result<(), reverie_rpc_transport::RpcError>>>,
    pub(crate) shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) connections: reverie_rpc_transport::ConnectionMonitor,
    pub(crate) issues: reverie_rpc_transport::RpcIssueMonitor,
    pub(crate) owner: Arc<MappedFirstAdmission>,
}
impl Drop for Serving {
    fn drop(&mut self) {
        // Emergency path retains failure and stops owned tasks; Child Drop still
        // kills/reaps. A successful test MUST take explicit joined cleanup below.
        if self.task.is_some() {
            self.owner
                .fail(&"physical admission test aborted before joined cleanup");
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }
}
impl Serving {
    pub(crate) async fn close(&mut self) -> io::Result<()> {
        // Children have already been killed/reaped or exited. Require actual
        // physical watcher drain and actual per-connection drain before shutdown.
        self.owner
            .exit_admission()
            .drain(WAIT)
            .await
            .map_err(|error| rpc(format!("physical watch drain failed: {error:?}")))?;
        bounded(async {
            self.connections.wait_for_idle().await;
            Ok(())
        })
        .await?;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let mut task = self
            .task
            .take()
            .ok_or_else(|| io::Error::other("server already closed"))?;
        let joined = tokio::time::timeout(WAIT, &mut task).await;
        match joined {
            Ok(result) => result.map_err(rpc)?.map_err(rpc),
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(rpc(error))
            }
        }
    }
}

/// The future (and its RpcServer pathname cleanup) drops before the retained
/// directory FD, including cancellation before the first poll. This prevents
/// an aborted server from unlinking through a later reuse of that descriptor.
pub(crate) struct DirectoryBoundServer<F> {
    pub(crate) server: std::pin::Pin<Box<F>>,
    pub(crate) _directory_fd: std::fs::File,
}
impl<F: std::future::Future> std::future::Future for DirectoryBoundServer<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.get_mut().server.as_mut().poll(context)
    }
}

pub(crate) struct Admission {
    pub(crate) owner: Arc<MappedFirstAdmission>,
    pub(crate) server: Option<RpcServer<detcore::GlobalState>>,
    pub(crate) issues: Option<reverie_rpc_transport::RpcIssueMonitor>,
    pub(crate) drained: bool,
}
impl Drop for Admission {
    fn drop(&mut self) {
        // Normal explicit drain runs first. Abort/unwind must at least poison
        // the actual watch; the original held ptracer still owns terminal wait.
        if !self.drained {
            self.owner
                .fail(&"held packet control ended before explicit watch drain");
        }
    }
}
#[cfg(test)]
pub(crate) async fn admit(
    admissions: &mut Vec<Admission>,
    global: &Arc<detcore::GlobalState>,
    config: &detcore::Config,
    socket: &std::path::Path,
    stop: &InitialExecStop<'_>,
    detlog: &UnixStream,
) -> io::Result<Arc<MappedFirstAdmission>> {
    let server = RpcServer::bind_exclusive(socket, global.clone(), config.clone()).map_err(rpc)?;
    let owner = MappedFirstAdmission::prepare(
        global.clone(),
        stop.duplicate_birth_pidfd().map_err(rpc)?,
        socket.as_os_str().as_bytes(),
        detlog.as_fd(),
        detlog.as_raw_fd(),
    )
    .await?;
    retain_admission(admissions, server, owner)
}

pub(crate) async fn admit_with_observer(
    admissions: &mut Vec<Admission>,
    global: &Arc<detcore::GlobalState>,
    config: &detcore::Config,
    socket: &std::path::Path,
    stop: &InitialExecStop<'_>,
    detlog: &UnixStream,
    observer: &UnixStream,
) -> io::Result<Arc<MappedFirstAdmission>> {
    let server = RpcServer::bind_exclusive(socket, global.clone(), config.clone()).map_err(rpc)?;
    let owner = MappedFirstAdmission::prepare_with_observer(
        global.clone(),
        stop.duplicate_birth_pidfd().map_err(rpc)?,
        socket.as_os_str().as_bytes(),
        detlog.as_fd(),
        observer.as_fd(),
    )
    .await?;
    retain_admission(admissions, server, owner)
}

fn retain_admission(
    admissions: &mut Vec<Admission>,
    server: RpcServer<detcore::GlobalState>,
    owner: Arc<MappedFirstAdmission>,
) -> io::Result<Arc<MappedFirstAdmission>> {
    // Retain real watch/server before any further fallible configuration.
    admissions.push(Admission {
        owner: owner.clone(),
        server: Some(server),
        issues: None,
        drained: false,
    });
    let retained = admissions
        .last_mut()
        .ok_or_else(|| rpc("admission slot absent"))?;
    retained.issues = Some(
        owner.configure_server(
            retained
                .server
                .as_mut()
                .ok_or_else(|| rpc("actual RPC server absent"))?,
        )?,
    );
    Ok(owner)
}

/// The packet storage check, over the packet descriptor `fd`, the bytes the
/// fill `retained`, and the admission's `expected` packet. It neither poisons
/// the lease nor fails the admission. `retained` and `expected` are separate
/// operands: passing the fill's bytes as both would make the first check true.
pub(crate) fn exact_storage(
    fd: BorrowedFd<'_>,
    retained: &[u8],
    expected: &[u8],
) -> io::Result<()> {
    need(
        retained == expected,
        "lease differs from actual initial_packet",
    )?;
    // Duplicate is a real shared-OFD alias for observation; positional IO leaves
    // its original shared offset unchanged. No path or replacement is used.
    let file = File::from(fd.try_clone_to_owned()?);
    need(
        file.metadata()?.len() == expected.len() as u64,
        "packet file length differs",
    )?;
    let mut bytes = vec![0; expected.len()];
    file.read_exact_at(&mut bytes, 0)?;
    need(bytes == expected, "sealed inherited OFD content differs")?;
    let mut tail = [0];
    need(
        file.read_at(&mut tail, expected.len() as u64)? == 0,
        "packet EOF differs",
    )?;
    // SAFETY: read-only fcntl/offset queries on the retained shared OFD.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    let offset = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
    need(
        seals == (libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL),
        "packet immutable seal set differs",
    )?;
    need(offset == 0, "packet inherited shared offset differs")
}

pub(crate) mod init_trial;

/// Pins `reserved_signals`' exact output. It runs at A(P)-1, where the function
/// is a test-only copy of init_trial's inline computation, and at A(P)-2, where
/// both init_trial call sites use it; equal output at both commits shows the
/// refactor changes nothing.
#[cfg(test)]
mod reserved_signals_tests {
    use super::reserved_signals;

    const SIGSYS_BIT: u64 = 1 << 30;
    const SIGSEGV_BIT: u64 = 1 << 10;

    #[test]
    fn reserved_signals_are_exact_for_every_config_init_trial_sees() {
        assert_eq!(libc::SIGSYS, 31);
        assert_eq!(libc::SIGSEGV, 11);
        // mapped_run's and mapped_cancel's configs: the default subscription
        // traps RDTSC, so SIGSEGV is reserved, with or without CPUID.
        let ordinary = detcore::Config {
            no_rcb_time: true,
            max_timeslice: None,
            target_timeslice: None,
            sequentialize_threads: false,
            in_guest_detlog_forward_policy: Some("INFO".to_owned()),
            ..Default::default()
        };
        let cases = [
            (
                "default",
                detcore::Config::default(),
                SIGSYS_BIT | SIGSEGV_BIT,
            ),
            ("ordinary", ordinary.clone(), SIGSYS_BIT | SIGSEGV_BIT),
            (
                "virtualize_cpuid",
                detcore::Config {
                    virtualize_cpuid: true,
                    ..ordinary.clone()
                },
                SIGSYS_BIT | SIGSEGV_BIT,
            ),
            (
                "no virtualize_cpuid",
                detcore::Config {
                    virtualize_cpuid: false,
                    ..ordinary.clone()
                },
                SIGSYS_BIT | SIGSEGV_BIT,
            ),
            // passthru_opt still traps RDTSC under virtual time, and CPUID
            // when it is virtualized; with neither, SIGSYS alone is reserved.
            (
                "passthru_opt",
                detcore::Config {
                    passthru_opt: true,
                    ..ordinary.clone()
                },
                SIGSYS_BIT | SIGSEGV_BIT,
            ),
            (
                "passthru_opt, no virtual time, no virtualize_cpuid",
                detcore::Config {
                    passthru_opt: true,
                    virtualize_time: false,
                    virtualize_cpuid: false,
                    ..ordinary.clone()
                },
                SIGSYS_BIT,
            ),
            (
                "passthru_opt, no virtual time, virtualize_cpuid",
                detcore::Config {
                    passthru_opt: true,
                    virtualize_time: false,
                    virtualize_cpuid: true,
                    ..ordinary
                },
                SIGSYS_BIT | SIGSEGV_BIT,
            ),
        ];
        for (name, config, expected) in cases {
            let actual = reserved_signals(&config);
            eprintln!("reserved_signals[{name}] = {actual:#x}");
            assert_eq!(actual, expected, "{name}");
        }
    }
}

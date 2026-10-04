//! Controlled provider premise across actual retained transport and ACK.
//! No BPF, GETREGSET or native Close is executed by this fixture.
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use std::time::Instant;

use super::*;
use crate::network_runtime::accepted_provider::CallStatus;
use crate::network_runtime::accepted_provider::Observation;
use crate::network_runtime::accepted_transport::AcceptedSession;
use crate::network_runtime::accepted_transport::Received;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Supported,
    Unsafe,
    FailedAck,
}

struct PublishedRefusal {
    error: String,
    history: super::super::accepted_controller::FixtureHistory,
}

pub(crate) struct Service {
    controller: Arc<Controller>,
    inherited_history: Option<super::super::accepted_controller::FixtureHistory>,
    stop: Arc<AtomicBool>,
    phases: Arc<AtomicUsize>,
    original_rejections: Arc<AtomicUsize>,
    published_refusal: Arc<Mutex<Option<PublishedRefusal>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Service {
    fn finish_retaining_debt(
        mut self,
        phases: usize,
        history: &super::super::accepted_controller::FixtureHistory,
    ) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
        assert_eq!(self.phases.load(Ordering::Acquire), phases);
        assert_eq!(self.original_rejections.load(Ordering::Acquire), 0);
        assert!(self.published_refusal.lock().unwrap().is_none());
        assert!(!self.controller.quiescent().unwrap());
        assert!(
            self.controller
                .current_close_fixture_matches_history(history)
        );
    }
    pub(crate) fn finish(mut self, groups: usize) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
        assert_eq!(self.phases.load(Ordering::Acquire), groups * 3);
        assert_eq!(self.original_rejections.load(Ordering::Acquire), 0);
        assert!(self.published_refusal.lock().unwrap().is_none());
        if let Some(history) = &self.inherited_history {
            assert!(
                self.controller
                    .current_close_fixture_preserves_history(history)
            );
            // This proves profile-group retirement and preservation of the
            // original birth request ledger, not global provider quiescence.
        } else {
            assert!(self.controller.quiescent().unwrap());
        }
    }
    pub(crate) fn finish_after_original_refusal(mut self, groups: usize) {
        assert!(self.inherited_history.is_none());
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
        assert_eq!(self.phases.load(Ordering::Acquire), groups * 3);
        assert_eq!(self.original_rejections.load(Ordering::Acquire), 1);
        let published = self.published_refusal.lock().unwrap().take().unwrap();
        assert!(
            self.controller
                .current_close_fixture_matches_history(&published.history)
        );
        assert_eq!(
            self.controller.quiescent().unwrap_err().to_string(),
            published.error
        );
        // The exact original preparation remains retained. This is not a
        // successful native Close or clean terminal-ownership claim.
    }
    pub(crate) fn original_rejections(&self) -> usize {
        self.original_rejections.load(Ordering::Acquire)
    }
    pub(crate) fn finish_detach(mut self, runtime: &mut NetworkRuntimeResources, groups: usize) {
        assert!(
            self.inherited_history.is_none(),
            "never detach an inherited birth owner"
        );
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
        assert_eq!(self.phases.load(Ordering::Acquire), groups * 3);
        assert!(self.published_refusal.lock().unwrap().is_none());
        assert!(self.controller.quiescent().unwrap());
        assert!(self.controller.current_close_fixture_history_empty());
        let mut c = runtime.shared.controller.lock().unwrap();
        assert!(matches!(c.as_ref(),Some(Ok(actual)) if Arc::ptr_eq(actual,&self.controller)));
        *c = None;
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            w.join().unwrap();
        }
    }
}
fn status(operation: &str) -> CallStatus {
    CallStatus {
        operation: operation.into(),
        returned: 0,
        errno: None,
    }
}
fn original_refusal() -> Observation<u64> {
    Observation {
        status: CallStatus {
            operation: "ap_prepare_original_close".into(),
            returned: -1,
            errno: Some(libc::EIO),
        },
        raw: 0,
    }
}
pub(crate) fn install(runtime: &mut NetworkRuntimeResources, root: Arc<ForegroundRoot>) -> Service {
    install_mode(runtime, root, Mode::Supported)
}
fn install_mode(
    runtime: &mut NetworkRuntimeResources,
    root: Arc<ForegroundRoot>,
    mode: Mode,
) -> Service {
    {
        let old = runtime.shared.controller.lock().unwrap();
        if let Some(old) = old.as_ref() {
            let old = old
                .as_ref()
                .expect("existing controller failure remains authoritative");
            assert!(
                old.quiescent().unwrap(),
                "cannot replace unresolved original controller"
            );
            assert!(
                old.current_close_fixture_history_empty(),
                "cannot erase existing controller history"
            );
        }
    }
    // Fixture setup must precede sharing/starting workers. Refuse rather than
    // unsafely mutate a runtime whose original owners are already active.
    let shared = Arc::get_mut(&mut runtime.shared)
        .expect("install controlled profile peer before runtime sharing");
    assert!(shared.driver.lock().unwrap().is_none());
    let mut pair = [-1; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
                pair.as_mut_ptr(),
            )
        },
        0
    );
    let wire = super::super::ProviderWireFormat::Abi12Copy5;
    let controller = Arc::new(
        Controller::from_startup(unsafe { OwnedFd::from_raw_fd(pair[0]) }, [7; 16], wire).unwrap(),
    );
    let service =
        AcceptedSession::from_wire(unsafe { OwnedFd::from_raw_fd(pair[1]) }, [7; 16], wire)
            .unwrap();
    shared.copy_wire = Some(wire);
    *shared.controller.lock().unwrap() = Some(Ok(controller.clone()));
    serve(
        controller,
        root,
        service,
        |peer| peer,
        Some(runtime.shared.clone()),
        None,
        mode,
    )
}

/// Continue the same already-started controlled provider connection. The peer's
/// original owner remains inside the worker and retains its driver Drop duty.
pub(in crate::network_runtime) fn serve_existing<P: Send + 'static>(
    runtime: &NetworkRuntimeResources,
    root: Arc<ForegroundRoot>,
    peer: P,
    session: fn(&mut P) -> &mut AcceptedSession,
) -> Service {
    assert_eq!(
        runtime.shared.copy_wire,
        Some(super::super::ProviderWireFormat::Abi12Copy5)
    );
    let controller = runtime.accepted_controller().unwrap();
    let history = controller.current_close_fixture_history();
    assert!(controller.current_close_fixture_preserves_history(&history));
    assert!(matches!(
        runtime.shared.driver.lock().unwrap().as_ref(),
        Some(Ok(_))
    ));
    serve(
        controller,
        root,
        peer,
        session,
        None,
        Some(history),
        Mode::Supported,
    )
}

fn serve<P: Send + 'static>(
    controller: Arc<Controller>,
    root: Arc<ForegroundRoot>,
    mut peer: P,
    session: fn(&mut P) -> &mut AcceptedSession,
    drive_owner: Option<Arc<super::super::RuntimeShared>>,
    inherited_history: Option<super::super::accepted_controller::FixtureHistory>,
    mode: Mode,
) -> Service {
    let phases = Arc::new(AtomicUsize::new(0));
    let counted = phases.clone();
    let original_rejections = Arc::new(AtomicUsize::new(0));
    let rejected = original_rejections.clone();
    let published_refusal = Arc::new(Mutex::new(None));
    let published = published_refusal.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let driver = controller.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut original: Option<Intent> = None;
        let mut correlation = 0;
        let mut seq = Vec::new();
        // Standalone peers own the same retained-effect callback as the real
        // Driver. Shared peers already have their original Driver owner.
        let drive = || {
            let Some(shared) = &drive_owner else {
                return true;
            };
            match driver.drive_once_retained(|| shared.retain_completed_collections(&driver)) {
                Ok(()) => true,
                Err(error) => {
                    assert_eq!(rejected.load(Ordering::Acquire), 1);
                    assert!(driver.current_close_fixture_original_refusal().unwrap());
                    assert_eq!(
                        error.to_string(),
                        format!("original preparation unresolved: {:?}", original_refusal())
                    );
                    let history = driver.current_close_fixture_history();
                    // Preserve the actual callback error in the controller,
                    // just as Driver::start does; never inject a caller error.
                    driver.fail(&error);
                    *published.lock().unwrap() = Some(PublishedRefusal {
                        error: error.to_string(),
                        history,
                    });
                    false
                }
            }
        };
        while !done.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "controlled profile peer exceeded fixture bound"
            );
            if !drive() {
                break;
            }
            let service = session(&mut peer);
            if let Some(Received::Request(sequence)) = service.try_receive().unwrap() {
                let (envelope, rights, _) = service.retained_request(sequence).unwrap();
                let request: Request = serde_json::from_slice(&envelope.body).unwrap();
                assert_eq!(envelope.owner, Some(root.owner()));
                let mut is_profile = true;
                let reply = match request {
                    Request::PrepareCurrentCloseProfile { call, intent } => {
                        assert!(original.is_none());
                        assert!(intent.valid_unarmed());
                        assert_eq!(rights.len(), 1);
                        assert_ne!(call, 0);
                        correlation = call;
                        original = Some(intent);
                        seq = vec![sequence];
                        Reply::Prepared(Observation {
                            status: status("ap_prepare_current_close_profile"),
                            raw: 7,
                        })
                    }
                    Request::CollectCurrentCloseProfile {
                        call,
                        command,
                        prepared_request,
                    } => {
                        let mut intent = original.clone().unwrap();
                        assert!(rights.is_empty());
                        assert_eq!((call, command, prepared_request), (correlation, 7, seq[0]));
                        intent.command = command;
                        seq.push(sequence);
                        let (provider, task, start, _) = root.native_identity();
                        let mut observed =
                            wire::controlled_collection(intent, provider, task, start);
                        if mode == Mode::Unsafe {
                            observed.raw.receipt.problem = 4;
                            observed.raw.receipt.entered.linger = 1;
                            observed.raw.receipt.returned.linger = 1;
                            let support = observed.raw.support.as_mut().unwrap();
                            support.returned = -1;
                            support.errno = Some(libc::EPROTO);
                        }
                        Reply::CurrentCloseProfile(observed)
                    }
                    Request::RetireCurrentCloseProfile {
                        call,
                        prepared,
                        completed,
                    } => {
                        assert!(rights.is_empty());
                        assert_eq!((call, prepared, completed), (correlation, seq[0], seq[1]));
                        service
                            .check_incoming_current_close_profile(
                                root.owner(),
                                call,
                                prepared,
                                completed,
                            )
                            .unwrap();
                        seq.push(sequence);
                        let mut ack = status("ap_ack_command");
                        if mode == Mode::FailedAck {
                            ack.returned = -1;
                            ack.errno = Some(libc::EIO);
                        }
                        Reply::CurrentCloseProfileRetired(ack)
                    }
                    Request::PrepareOriginalConnect { kind, call, .. } => {
                        assert!(original.is_none());
                        assert_eq!(kind, crate::network_replay::original_connect::Kind::Close);
                        assert_ne!(call, 0);
                        assert_eq!(rights.len(), 1);
                        is_profile = false;
                        rejected.fetch_add(1, Ordering::AcqRel);
                        Reply::Prepared(original_refusal())
                    }
                    _ => panic!("unexpected controlled profile request"),
                };
                service
                    .dispatch(sequence, |_, _| Ok(serde_json::to_vec(&reply).unwrap()))
                    .unwrap();
                if is_profile && seq.len() == 3 && mode != Mode::FailedAck {
                    service
                        .retire_incoming_current_close_profile(
                            root.owner(),
                            correlation,
                            [seq[0], seq[1], seq[2]],
                        )
                        .unwrap();
                    original = None;
                }
                assert!(service.try_reply(sequence).unwrap());
                if !drive() {
                    break;
                }
                if is_profile {
                    counted.fetch_add(1, Ordering::AcqRel);
                }
            }
            std::thread::yield_now();
        }
    });
    Service {
        controller,
        inherited_history,
        stop,
        phases,
        original_rejections,
        published_refusal,
        worker: Some(worker),
    }
}
#[tokio::test]
async fn current_close_runtime_consumes_one_trigger_and_requires_real_group_ack() {
    let mut f = crate::network_replay::shared_send::tests::fixture();
    let service = install(&mut f.runtime, f.root.clone());
    let file = FileIdentity::controlled_fixture(f.root.native_identity().0, 37);
    let raw = [f.read.fd as usize, 0, 0, 0, 0, 0];
    let prepared = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap();
    assert!(!prepared.settled());
    let duplicate = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await;
    assert!(
        duplicate.is_err(),
        "same response cannot issue a second native trigger"
    );
    let completed = f
        .runtime
        .collect_current_close_profile(&prepared, true)
        .await
        .unwrap();
    assert!(prepared.settled());
    let repeated_after_ack = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap_err();
    assert_eq!(
        repeated_after_ack.to_string(),
        "retired current Close observation cannot be submitted again"
    );
    // The peer must still see exactly one preparation/collection/ACK group.
    completed.validate_read(&f.root, 0, &f.read, raw).unwrap();
    assert!(completed.validate_read(&f.root, 1, &f.read, raw).is_err());
    for index in 0..6 {
        let mut changed = raw;
        changed[index] ^= 1;
        assert!(
            completed
                .validate_read(&f.root, 0, &f.read, changed)
                .is_err()
        );
    }
    assert!(
        f.runtime
            .collect_current_close_profile(&prepared, true)
            .await
            .is_err()
    );
    service.finish(1);
}

#[tokio::test]
async fn current_close_runtime_unsafe_receipt_acks_before_refusing_foreground() {
    let mut f = crate::network_replay::shared_send::tests::fixture();
    let service = install_mode(&mut f.runtime, f.root.clone(), Mode::Unsafe);
    let file = FileIdentity::controlled_fixture(f.root.native_identity().0, 37);
    let raw = [f.read.fd as usize, 0, 0, 0, 0, 0];
    let prepared = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap();
    let error = f
        .runtime
        .collect_current_close_profile(&prepared, true)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "current Close physical profile is unsupported; logical foreground selection is retained"
    );
    assert!(prepared.settled());
    assert!(
        service
            .controller
            .current_close_fixture_requests()
            .is_empty()
    );
    service.finish(1);
}

#[tokio::test]
async fn current_close_runtime_failed_ack_preserves_exact_command_and_unsettled_owner() {
    let mut f = crate::network_replay::shared_send::tests::fixture();
    let service = install_mode(&mut f.runtime, f.root.clone(), Mode::FailedAck);
    let file = FileIdentity::controlled_fixture(f.root.native_identity().0, 37);
    let raw = [f.read.fd as usize, 0, 0, 0, 0, 0];
    let prepared = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap();
    let error = f
        .runtime
        .collect_current_close_profile(&prepared, true)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "current Close ACK failed; exact debt retained"
    );
    assert!(!prepared.settled());
    let requests = service.controller.current_close_fixture_requests();
    let [
        Request::PrepareCurrentCloseProfile {
            call: pcall,
            intent,
        },
        Request::CollectCurrentCloseProfile {
            call: ccall,
            command,
            prepared_request,
        },
        Request::RetireCurrentCloseProfile {
            call: rcall,
            prepared: first,
            completed,
        },
    ] = requests.as_slice()
    else {
        panic!("exact unresolved profile request group");
    };
    assert_eq!(
        (*pcall, *ccall, *rcall),
        (
            f.read.publication.permit.native_command_call(),
            f.read.publication.permit.native_command_call(),
            f.read.publication.permit.native_command_call()
        )
    );
    let mut unarmed = prepared.intent.clone();
    unarmed.command = 0;
    assert_eq!(*intent, unarmed);
    assert_eq!(
        (*command, *prepared_request, *first),
        (
            prepared.intent.command,
            prepared.prepared,
            prepared.prepared
        )
    );
    assert!(*completed > *first);
    let history = service.controller.current_close_fixture_history();
    drop(prepared);
    let error = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "current Close trigger was already issued or changed preparation"
    );
    service.finish_retaining_debt(3, &history);
}

#[tokio::test]
async fn current_close_runtime_dropped_preparation_cannot_rearm_or_claim_quiescence() {
    let mut f = crate::network_replay::shared_send::tests::fixture();
    let service = install(&mut f.runtime, f.root.clone());
    let file = FileIdentity::controlled_fixture(f.root.native_identity().0, 37);
    let raw = [f.read.fd as usize, 0, 0, 0, 0, 0];
    let prepared = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap();
    assert!(!prepared.settled());
    let requests = service.controller.current_close_fixture_requests();
    let [Request::PrepareCurrentCloseProfile { call, intent }] = requests.as_slice() else {
        panic!("original preparation remains the only request");
    };
    assert_eq!(*call, f.read.publication.permit.native_command_call());
    let mut unarmed = prepared.intent.clone();
    unarmed.command = 0;
    assert_eq!(*intent, unarmed);
    let history = service.controller.current_close_fixture_history();
    drop(prepared);
    let error = f
        .runtime
        .prepare_current_close_profile(f.root.clone(), 0, f.read.clone(), raw, file)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "current Close trigger was already issued or changed preparation"
    );
    service.finish_retaining_debt(1, &history);
}

//! Exact original TX workers live on the existing Call. Replies are not joins.
use std::sync::Arc;

use super::*;
use crate::network_replay::shared_send::SharedRecordSend;

#[derive(Debug, Default)]
pub(crate) struct SendWorkers {
    state: Mutex<Workers>,
}
#[derive(Debug, Default)]
struct Workers {
    prepare: Option<NativeWorkerHandle>,
    prepare_join: Option<JoinedNativeWorkerReceipt>,
    close: Option<NativeWorkerHandle>,
    close_join: Option<JoinedNativeWorkerReceipt>,
    // Sticky physical-cleanup disposition; no later join can upgrade it.
    cleanup_only: Option<String>,
}
impl SendWorkers {
    fn bind(&self, worker: NativeWorkerHandle, close: bool) -> std::io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if close {
            if s.prepare_join.is_none() || s.close.is_some() {
                return Err(std::io::Error::other(
                    "shared send close lacks original joined preparation",
                ));
            }
            s.close = Some(worker);
        } else {
            if s.prepare.is_some() || s.close.is_some() {
                return Err(std::io::Error::other("shared send preparation repeated"));
            }
            s.prepare = Some(worker);
        }
        Ok(())
    }
    fn bind_retirement(
        &self,
        worker: NativeWorkerHandle,
        publication: std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut s = self.state.lock().unwrap();
        if s.close.is_some() {
            return Err(std::io::Error::other("shared send close owner repeated"));
        }
        s.cleanup_only = publication.err().map(|error| error.to_string());
        s.close = Some(worker);
        Ok(())
    }
    fn prepared(&self, runtime: &Arc<RuntimeShared>) -> std::io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .prepare_join
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shared send preparation is not joined"))?
            .validate_runtime(runtime)
    }
    async fn join(&self, runtime: &Arc<RuntimeShared>, close: bool) -> std::io::Result<()> {
        let worker = {
            let s = self.state.lock().unwrap();
            if close { &s.close } else { &s.prepare }.clone()
        }
        .ok_or_else(|| std::io::Error::other("shared send lost original worker handle"))?;
        let receipt = runtime.join_native_worker_receipt(&worker).await?;
        let mut s = self.state.lock().unwrap();
        let retained = if close { &s.close } else { &s.prepare };
        if retained.as_ref().is_none_or(|w| !Arc::ptr_eq(w, &worker)) {
            return Err(std::io::Error::other(
                "shared send worker replaced across join",
            ));
        }
        let slot = if close {
            &mut s.close_join
        } else {
            &mut s.prepare_join
        };
        if slot.is_none() {
            *slot = Some(receipt);
        }
        Ok(())
    }
    fn complete(&self, runtime: &Arc<RuntimeShared>) -> std::io::Result<()> {
        let s = self.state.lock().unwrap();
        if let Some(reason) = &s.cleanup_only {
            return Err(std::io::Error::other(format!(
                "shared send cleanup cannot authorize publication: {reason}"
            )));
        }
        s.prepare_join
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shared send lacks preparation join"))?
            .validate_runtime(runtime)?;
        s.close_join
            .as_ref()
            .ok_or_else(|| std::io::Error::other("shared send lacks close join"))?
            .validate_runtime(runtime)
    }
}

pub(crate) struct CompletedSharedNativeSend<'a> {
    pub(in crate::network_runtime) origin: &'a Arc<SharedRecordSend>,
    pub(in crate::network_runtime) capture: &'a original_send::BlockingCapture,
    _workers: &'a NativeWorkers,
    _calls: &'a native_peer::Calls,
}
impl CompletedSharedNativeSend<'_> {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordSend> {
        self.origin
    }
    pub(crate) fn capture(&self) -> std::io::Result<&original_send::BlockingCapture> {
        Ok(self.capture)
    }
}
impl RuntimeShared {
    fn check_send_workers(
        &self,
        origin: &SharedRecordSend,
        owned: &NativeWorkers,
        submitted: u64,
    ) -> std::io::Result<()> {
        let prefix = origin.prefix().native_prefix();
        let actual = prefix
            .shared
            .upgrade()
            .ok_or_else(|| std::io::Error::other("shared send runtime gone"))?;
        if !std::ptr::eq(actual.as_ref(), self)
            || owned.closed
            || owned.copy_exclusion.is_some()
            || owned.source_read_active()
            || owned.submission_generation
                != prefix.generation.checked_add(submitted).ok_or_else(|| {
                    std::io::Error::other("shared send worker generation overflow")
                })?
            || !origin.root().is_current(origin.owner())
            || !origin.root().has_shared_mm_history()
        {
            return Err(std::io::Error::other(
                "shared send changed fixed worker prefix",
            ));
        }
        if let Some(e) = self.native_terminal_failure.lock().unwrap().as_ref() {
            return Err(std::io::Error::other(e.clone()));
        }
        Ok(())
    }
    pub(super) fn start_shared_send_close<T: Send + 'static>(
        self: &Arc<Self>,
        origin: Arc<SharedRecordSend>,
        cleanup: bool,
        executor: tokio::runtime::Handle,
        operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
    ) -> std::io::Result<()> {
        let (arm, gate) = std::sync::mpsc::channel();
        let (worker, _receive) = self.start_original_retirement_worker(
            origin.owner(),
            origin.admission().call,
            executor,
            move || {
                gate.recv()
                    .map_err(|_| std::io::Error::other("shared send close was not armed"))?;
                operation()
            },
        )?;
        let owned = self.native_workers.lock().unwrap();
        let publication = (|| {
            if cleanup {
                return Err(std::io::Error::other(
                    "original send has canceled or terminal cleanup custody",
                ));
            }
            origin.workers.prepared(self)?;
            self.check_send_workers(&origin, &owned, 2)?;
            if owned.tasks.len() != 1 || !Arc::ptr_eq(&owned.tasks[0], &worker) {
                return Err(std::io::Error::other(
                    "shared send close worker population changed",
                ));
            }
            Ok(())
        })();
        // The original retired Call authorizes physical close even after
        // cutoff, revoked root or lost callback. Those facts cannot authorize
        // trace publication. Retain the distinction before opening this gate.
        if let Err(error) = origin.workers.bind_retirement(worker, publication) {
            self.native_terminal_failure
                .lock()
                .unwrap()
                .get_or_insert_with(|| error.to_string());
            // Dropping arm wakes the registered worker with a retained error.
            // Its actual JoinHandle stays in the existing terminal registry.
            return Err(error);
        }
        arm.send(())
            .map_err(|_| std::io::Error::other("shared send close worker lost gate"))
    }
}
impl NetworkRuntimeResources {
    pub(crate) async fn prepare_shared_original_send(
        &self,
        origin: Arc<SharedRecordSend>,
        task: OwnedFd,
        publication: NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        let owner = origin.owner();
        let admission = origin.admission().clone();
        let controller = self.accepted_controller()?;
        let executor = tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?;
        {
            let engine = publication.engine.lock().unwrap();
            if !Arc::ptr_eq(
                &engine
                    .shared_record_send_origin(&admission)
                    .map_err(std::io::Error::other)?,
                &origin,
            ) || !origin
                .prefix()
                .matches_retained_peers(&engine, Some(admission.call))
                .map_err(std::io::Error::other)?
            {
                return Err(std::io::Error::other(
                    "shared send preparation changed original Call/census",
                ));
            }
            let owned = self.shared.native_workers.lock().unwrap();
            self.shared.check_send_workers(&origin, &owned, 0)?;
            if !owned.tasks.is_empty() {
                return Err(std::io::Error::other(
                    "shared send prefix contains unjoined workers",
                ));
            }
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_send(origin.prefix().peers(), &origin, 0)?;
        }
        let (arm, gate) = std::sync::mpsc::channel();
        let shared = self.shared.clone();
        let retained = origin.clone();
        let authority = publication.clone();
        let admitted = admission.clone();
        let selected_executor = executor.clone();
        let (worker, receive) = self.shared.start_native_worker(executor, move || {
            gate.recv()
                .map_err(|_| std::io::Error::other("shared send preparation was not armed"))?;
            let pin = match capture_socket(&task, admitted.arguments.fd) {
                Ok(pin) => pin,
                Err(error) => {
                    authority.retire_original_lifetime(owner, &admitted, true)?;
                    return Err(error);
                }
            };
            shared.native_streams.lock().unwrap().capture_original(
                owner,
                admitted.clone(),
                Some(pin),
                selected_executor,
                authority.clone(),
            )?;
            shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admitted.call)?
                .shared_send = Some(retained.clone());
            let held = shared
                .native_streams
                .lock()
                .unwrap()
                .original_reference(owner, admitted.call)?
                .ok_or_else(|| std::io::Error::other("shared send original pin absent"))?;
            let checked = (|| {
                let observed = original_send::classify_blocking(&held)?;
                if observed.ticks() != retained.timeout() {
                    return Err(std::io::Error::other(
                        "blocking send timeout changed from selected intent",
                    ));
                }
                let classified = native_peer::classify_original(&held)?;
                shared.native_streams.lock().unwrap().original_classified(
                    owner,
                    admitted.call,
                    classified,
                )
            })();
            drop(held);
            if let Err(error) = checked {
                shared.retire_original_before_submission(owner, &admitted, &authority)?;
                return Err(error);
            }
            let canceled = {
                let mut engine = authority.engine.lock().unwrap();
                let canceled = engine
                    .original_connect_cancellation(owner, &admitted)
                    .map_err(std::io::Error::other)?
                    .0;
                if !canceled {
                    engine
                        .original_connect_provider_submitted(owner, &admitted)
                        .map_err(std::io::Error::other)?;
                }
                canceled
            };
            if canceled {
                shared.retire_original_before_submission(owner, &admitted, &authority)?;
                return Ok(());
            }
            let a = &admitted.arguments;
            let sequence = controller.prepare(
                accepted_controller::Effect::PrepareOriginalConnect(admitted.call),
                owner,
                &accepted_provider::Request::PrepareOriginalConnect {
                    kind: a.kind,
                    call: admitted.call.native_command_call(),
                    mm: owner.mm.generation(),
                    fd: a.fd,
                    address: a.address,
                    length: a.length,
                    original_count: a.original_count,
                },
                || Ok(vec![task.as_fd().try_clone_to_owned()?]),
            )?;
            shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admitted.call)?
                .prepare_request = Some(sequence);
            authority.changed.notify_waiters();
            Ok(())
        })?;
        {
            let engine = publication.engine.lock().unwrap();
            if !origin
                .prefix()
                .matches_retained_peers(&engine, Some(admission.call))
                .map_err(std::io::Error::other)?
            {
                return Err(std::io::Error::other(
                    "shared send peer changed before physical capture",
                ));
            }
            let owned = self.shared.native_workers.lock().unwrap();
            self.shared.check_send_workers(&origin, &owned, 1)?;
            if owned.tasks.len() != 1 || !Arc::ptr_eq(&owned.tasks[0], &worker) {
                return Err(std::io::Error::other(
                    "shared preparation worker population changed",
                ));
            }
            self.shared
                .native_streams
                .lock()
                .unwrap()
                .require_shared_send(origin.prefix().peers(), &origin, 0)?;
            origin.workers.bind(worker, false)?;
            arm.send(())
                .map_err(|_| std::io::Error::other("shared preparation worker lost gate"))?;
        }
        let result = receive.await;
        origin.workers.join(&self.shared, false).await?;
        result.map_err(std::io::Error::other)??;
        self.wait_original(owner, &admission, &publication, false)
            .await?;
        Ok(())
    }
    pub(crate) fn check_shared_send_prepared(
        &self,
        origin: &Arc<SharedRecordSend>,
    ) -> std::io::Result<()> {
        origin.workers.prepared(&self.shared)?;
        let owned = self.shared.native_workers.lock().unwrap();
        self.shared.check_send_workers(origin, &owned, 1)?;
        if !owned.tasks.is_empty() {
            return Err(std::io::Error::other(
                "shared send preparation worker remains live",
            ));
        }
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .require_shared_send(origin.prefix().peers(), origin, 1)?;
        Ok(())
    }
    pub(crate) async fn join_shared_original_send_completion(
        &self,
        origin: &Arc<SharedRecordSend>,
    ) -> std::io::Result<()> {
        origin.workers.join(&self.shared, true).await
    }
    pub(crate) fn with_shared_send_completion<T>(
        &self,
        origin: &Arc<SharedRecordSend>,
        engine: &mut crate::network_replay::NetworkReplayEngine,
        publish: impl FnOnce(
            &mut crate::network_replay::NetworkReplayEngine,
            &CompletedSharedNativeSend<'_>,
        ) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        origin.workers.complete(&self.shared)?;
        if !origin
            .prefix()
            .matches_retained_peers(engine, Some(origin.admission().call))
            .map_err(std::io::Error::other)?
        {
            return Err(std::io::Error::other(
                "shared send publication changed fixed peers",
            ));
        }
        let owned = self.shared.native_workers.lock().unwrap();
        self.shared.check_send_workers(origin, &owned, 2)?;
        if !owned.tasks.is_empty() {
            return Err(std::io::Error::other(
                "shared send publication has unjoined worker",
            ));
        }
        let calls = self.shared.native_streams.lock().unwrap();
        let capture = calls
            .require_shared_send(origin.prefix().peers(), origin, 2)?
            .ok_or_else(|| std::io::Error::other("shared send completion lacks typed capture"))?;
        publish(
            engine,
            &CompletedSharedNativeSend {
                origin,
                capture,
                _workers: &owned,
                _calls: &calls,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_send_replies_cannot_replace_both_actual_worker_joins() {
        let (runtime, _, _, _, _) =
            super::super::controlled_foreground_runtime(
                unsafe { libc::syscall(libc::SYS_gettid) } as i32
            );
        let workers = SendWorkers::default();
        let (prepare, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        assert!(workers.bind(prepare.clone(), true).is_err());
        workers.bind(prepare.clone(), false).unwrap();
        assert!(workers.bind(prepare, false).is_err());
        reply.await.unwrap().unwrap();
        assert!(
            workers.prepared(&runtime.shared).is_err(),
            "reply is not JoinHandle retirement"
        );
        assert!(workers.complete(&runtime.shared).is_err());
        workers.join(&runtime.shared, false).await.unwrap();
        workers.prepared(&runtime.shared).unwrap();
        assert!(
            workers.complete(&runtime.shared).is_err(),
            "preparation cannot stand in for close"
        );
        let (close, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || Ok(()))
            .unwrap();
        workers.bind(close.clone(), true).unwrap();
        assert!(workers.bind(close, true).is_err());
        reply.await.unwrap().unwrap();
        assert!(
            workers.complete(&runtime.shared).is_err(),
            "close reply is not its true join"
        );
        workers.join(&runtime.shared, true).await.unwrap();
        workers.complete(&runtime.shared).unwrap();
        let (foreign, _, _, _, _) =
            super::super::controlled_foreground_runtime(
                unsafe { libc::syscall(libc::SYS_gettid) } as i32
            );
        assert!(workers.complete(&foreign.shared).is_err());
        assert!(
            runtime
                .shared
                .native_workers
                .lock()
                .unwrap()
                .tasks
                .is_empty()
        );
    }

    #[tokio::test]
    async fn shared_send_failed_actual_preparation_never_issues_join_success() {
        let (runtime, _, _, _, _) =
            super::super::controlled_foreground_runtime(
                unsafe { libc::syscall(libc::SYS_gettid) } as i32
            );
        let workers = SendWorkers::default();
        let (prepare, reply) = runtime
            .shared
            .start_native_worker(tokio::runtime::Handle::current(), || {
                Err::<(), _>(std::io::Error::other("actual preparation failed"))
            })
            .unwrap();
        workers.bind(prepare, false).unwrap();
        assert!(reply.await.unwrap().is_err());
        assert!(workers.join(&runtime.shared, false).await.is_err());
        assert!(workers.prepared(&runtime.shared).is_err());
        assert!(workers.complete(&runtime.shared).is_err());
    }
}

#[cfg(test)]
mod cleanup_tests {
    use std::io::Read;
    use std::time::Duration;

    use super::*;
    use crate::network_replay::original_connect::Local;
    use crate::network_replay::original_connect::Pin;
    use crate::network_replay::shared_send::SharedRecordSendEntry;

    async fn selected(
        f: &crate::network_replay::shared_send::tests::Fixture,
    ) -> Arc<SharedRecordSend> {
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, None)
            .await
            .unwrap();
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut engine = f.engine.lock().unwrap();
                f.runtime
                    .with_shared_attempt_prefix(&prefix, &mut engine, |engine, held| {
                        engine
                            .begin_shared_record_send(
                                SharedRecordSendEntry {
                                    read: f.read.clone(),
                                    arguments: f.arguments.clone(),
                                    raw: f.raw,
                                },
                                &grant,
                                &prefix,
                                held,
                                f.now,
                            )
                            .map_err(std::io::Error::other)
                    })
            })
            .unwrap()
    }

    #[tokio::test]
    async fn shared_send_cleanup_closes_actual_pin_after_canceled_join_or_final_wait_and_cutoff() {
        // Controlled earlier provider preparation/retirement premises; actual
        // worker execution, future cancellation, joins and Unix peer EOF. No
        // case claims to execute a provider command or original TCP Sendto.
        for case in 0..5 {
            let f = crate::network_replay::shared_send::tests::fixture();
            let origin = selected(&f).await;
            let owner = origin.owner();
            let admission = origin.admission().clone();
            let trace = f.engine.lock().unwrap().native_trace_fixture();
            let engine = Arc::new(f.engine);
            let publication = NativeCaptureRecovery::new(
                engine.clone(),
                Arc::new(tokio::sync::Notify::new()),
                |_| {},
            );
            let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            f.runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .capture_original(
                    owner,
                    admission.clone(),
                    Some(pin.into()),
                    tokio::runtime::Handle::current(),
                    publication.clone(),
                )
                .unwrap();
            {
                let mut calls = f.runtime.shared.native_streams.lock().unwrap();
                let state = calls.original(owner, admission.call).unwrap();
                state.shared_send = Some(origin.clone());
                state.retired = true; // Explicit controlled positive provider-retirement premise.
                state.close_queued = true;
            }
            let (release, released) = std::sync::mpsc::channel();
            let (started, observed) = tokio::sync::oneshot::channel();
            let (prepare, reply) = f
                .runtime
                .shared
                .start_native_worker(tokio::runtime::Handle::current(), move || {
                    started.send(()).unwrap();
                    released
                        .recv_timeout(Duration::from_secs(1))
                        .map_err(std::io::Error::other)?;
                    Ok(())
                })
                .unwrap();
            origin.workers.bind(prepare, false).unwrap();
            observed.await.unwrap();
            {
                let mut e = engine.lock().unwrap();
                e.original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                e.original_call_prepared(
                    owner,
                    &admission,
                    Some(Pin::Socket {
                        domain: libc::AF_INET,
                        kind: libc::SOCK_STREAM,
                        protocol: libc::IPPROTO_TCP,
                    }),
                    17,
                )
                .unwrap();
                let local = Local {
                    arguments: admission.arguments.clone(),
                    raw_arguments: f.raw,
                    admission: Some(admission.clone()),
                    invoked: case != 0,
                    returned: None,
                };
                if case == 0 {
                    e.original_connect_consumed(owner, &local).unwrap();
                    e.original_connect_disarmed(owner, &admission, 17).unwrap();
                    e.original_connect_cancel_retired(owner, &admission)
                        .unwrap();
                } else {
                    e.original_connect_invoked(owner, &admission).unwrap();
                    assert!(e.original_connect_final_wait(owner, &local).unwrap());
                    e.original_connect_dead_retired(owner, &admission, 17)
                        .unwrap();
                    assert!(e.original_connect_task_terminal(owner, &admission).unwrap());
                }
                assert_eq!(e.original_connect_result(owner, &admission).unwrap(), None);
            }
            let mut release = Some(release);
            let mut reply = Some(reply);
            if case == 0 {
                let mut wait = Box::pin(origin.workers.join(&f.runtime.shared, false));
                assert!(futures::poll!(wait.as_mut()).is_pending());
                drop(wait); // The actual preparation JoinHandle remains owned.
                assert!(origin.workers.prepared(&f.runtime.shared).is_err());
            } else {
                release.take().unwrap().send(()).unwrap();
                reply.take().unwrap().await.unwrap().unwrap();
                origin.workers.join(&f.runtime.shared, false).await.unwrap();
            }
            if case == 1 || case == 2 {
                f.runtime.shared.native_workers.lock().unwrap().closed = true;
            }
            if case == 1 || case == 4 {
                f.root.revoke();
            }
            if case == 1 || case == 3 {
                *f.runtime.shared.native_terminal_failure.lock().unwrap() =
                    Some("retained primary failure".into());
            }
            let shared = f.runtime.shared.clone();
            let authority = publication.clone();
            let admitted = admission.clone();
            // Cases2..4 model cutoff/failure arriving after the Driver captured
            // a positive-intent snapshot but before the worker's gate opens.
            f.runtime
                .shared
                .start_shared_send_close(
                    origin.clone(),
                    case < 2,
                    tokio::runtime::Handle::current(),
                    move || {
                        let work = shared
                            .native_streams
                            .lock()
                            .unwrap()
                            .prepare_release(owner, admitted.call)?;
                        let raw = work.perform();
                        shared.native_streams.lock().unwrap().retain_release(
                            owner,
                            admitted.call,
                            raw,
                        )?;
                        authority
                            .engine
                            .lock()
                            .unwrap()
                            .original_connect_pin_released(owner, &admitted)
                            .map_err(std::io::Error::other)
                    },
                )
                .unwrap();
            tokio::time::timeout(
                Duration::from_secs(1),
                origin.workers.join(&f.runtime.shared, true),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                peer.read(&mut [0; 1]).unwrap(),
                0,
                "case{case}: actual final pin close"
            );
            assert!(
                f.runtime
                    .shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .original_closed(owner, admission.call)
                    .unwrap()
            );
            assert!(origin.workers.state.lock().unwrap().cleanup_only.is_some());
            if case == 0 {
                release.take().unwrap().send(()).unwrap();
                reply.take().unwrap().await.unwrap().unwrap();
                origin.workers.join(&f.runtime.shared, false).await.unwrap();
            }
            assert!(
                origin.workers.complete(&f.runtime.shared).is_err(),
                "cleanup cannot upgrade after later preparation join"
            );
            let entered = std::cell::Cell::new(false);
            let error = f
                .runtime
                .with_shared_send_completion(&origin, &mut engine.lock().unwrap(), |_, _| {
                    entered.set(true);
                    Ok(())
                })
                .expect_err("cleanup cannot enter the production publication borrower");
            assert!(
                error
                    .to_string()
                    .starts_with("shared send cleanup cannot authorize publication:"),
                "specific cleanup-only refusal: {error}"
            );
            assert!(!entered.get(), "cleanup entered publication");
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
            publication
                .retire_original_lifetime(owner, &admission, false)
                .unwrap();
            f.runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .finish_release(owner, admission.call)
                .unwrap();
            assert!(
                f.runtime
                    .shared
                    .native_workers
                    .lock()
                    .unwrap()
                    .tasks
                    .is_empty()
            );
            assert!(
                f.runtime
                    .shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .settled()
                    .is_ok()
            );
            assert!(origin.workers.complete(&f.runtime.shared).is_err());
            if case == 1 || case == 3 {
                assert_eq!(
                    f.runtime
                        .shared
                        .native_terminal_failure
                        .lock()
                        .unwrap()
                        .as_deref(),
                    Some("retained primary failure")
                );
            }
        }
    }

    #[tokio::test]
    async fn shared_send_missing_selected_with_same_count_foreign_native_row_refuses_without_mutation()
     {
        let f = crate::network_replay::shared_send::tests::fixture();
        let origin = selected(&f).await;
        let mut foreign = origin.admission().clone();
        foreign.call = crate::network_replay::NetworkStreamCallId::controlled_fixture(
            foreign.call.native_command_call() + 99,
        );
        let publication = NativeCaptureRecovery::new(
            Arc::new(f.engine),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let mut calls = f.runtime.shared.native_streams.lock().unwrap();
        calls
            .capture_original(
                origin.owner(),
                foreign.clone(),
                None,
                tokio::runtime::Handle::current(),
                publication,
            )
            .unwrap();
        for phase in [1, 2] {
            assert!(
                calls
                    .require_shared_send(origin.prefix().peers(), &origin, phase)
                    .is_err()
            );
            let rows = calls.originals();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1.admission, foreign);
        }
    }

    #[tokio::test]
    async fn shared_send_duplicate_close_keeps_failed_registered_gate_and_original_pin_outcome() {
        let f = crate::network_replay::shared_send::tests::fixture();
        let origin = selected(&f).await;
        let owner = origin.owner();
        let admission = origin.admission().clone();
        let publication = NativeCaptureRecovery::new(
            Arc::new(f.engine),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let (pin, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        {
            let mut calls = f.runtime.shared.native_streams.lock().unwrap();
            calls
                .capture_original(
                    owner,
                    admission.clone(),
                    Some(pin.into()),
                    tokio::runtime::Handle::current(),
                    publication,
                )
                .unwrap();
            let state = calls.original(owner, admission.call).unwrap();
            state.shared_send = Some(origin.clone());
            state.retired = true; // Controlled actual-provider-retirement premise.
            state.close_queued = true;
        }
        let shared = f.runtime.shared.clone();
        let call = admission.call;
        f.runtime
            .shared
            .start_shared_send_close(
                origin.clone(),
                true,
                tokio::runtime::Handle::current(),
                move || {
                    let work = shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .prepare_release(owner, call)?;
                    let raw = work.perform();
                    shared
                        .native_streams
                        .lock()
                        .unwrap()
                        .retain_release(owner, call, raw)
                },
            )
            .unwrap();
        origin.workers.join(&f.runtime.shared, true).await.unwrap();
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        assert!(
            f.runtime
                .shared
                .start_shared_send_close(
                    origin.clone(),
                    true,
                    tokio::runtime::Handle::current(),
                    move || {
                        flag.store(true, std::sync::atomic::Ordering::Release);
                        Ok(())
                    }
                )
                .is_err()
        );
        let worker = {
            let owned = f.runtime.shared.native_workers.lock().unwrap();
            assert_eq!(owned.tasks.len(), 1);
            owned.tasks[0].clone()
        };
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            f.runtime.shared.join_native_worker(&worker),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("close was not armed"));
        assert!(!ran.load(std::sync::atomic::Ordering::Acquire));
        assert!(worker.completion.lock().await.task.is_none());
        assert!(
            worker
                .completion
                .lock()
                .await
                .terminal
                .as_ref()
                .unwrap()
                .is_err()
        );
        assert_eq!(
            f.runtime.shared.native_workers.lock().unwrap().tasks.len(),
            1,
            "failed registered worker remains retained evidence"
        );
        assert!(
            f.runtime
                .shared
                .native_streams
                .lock()
                .unwrap()
                .original_closed(owner, call)
                .unwrap()
        );
        assert!(origin.workers.complete(&f.runtime.shared).is_err());
        assert_eq!(
            f.runtime
                .shared
                .native_terminal_failure
                .lock()
                .unwrap()
                .as_deref(),
            Some("shared send close owner repeated")
        );
    }
}

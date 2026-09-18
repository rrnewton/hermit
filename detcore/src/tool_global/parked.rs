/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Errno;

use super::*;

impl GlobalState {
    pub(super) async fn recv_request_resources(
        &self,
        from: Tid,
        pid: DetPid,
        resources: Resources,
        mm: Option<MmId>,
    ) -> (SchedulerRpcResult<ResumeStatus>, Option<LogicalTime>) {
        self.recv_grant_resources(from, pid, resources, mm, RpcOrigin::DirectRequestResources)
            .await
    }

    pub(super) async fn recv_grant_resources(
        &self,
        from: Tid,
        pid: DetPid,
        resources: Resources,
        mm: Option<MmId>,
        origin: RpcOrigin,
    ) -> (SchedulerRpcResult<ResumeStatus>, Option<LogicalTime>) {
        let (result, time) = self
            .recv_resources_with_origin(from, pid, resources, mm, origin, ControlCapability::None)
            .await;
        match result {
            SchedulerRpcResult::Continue(ResourceReply::Grant(status)) => {
                (SchedulerRpcResult::Continue(status), time)
            }
            SchedulerRpcResult::ThreadExited => (SchedulerRpcResult::ThreadExited, None),
            SchedulerRpcResult::Continue(ResourceReply::PublishAlarm(_)) => {
                self.sched.lock().unwrap().fail_parked(
                    DetTid::from_raw(from.as_raw()),
                    ProtocolFailure::UnexpectedControl,
                );
                (SchedulerRpcResult::ThreadExited, None)
            }
        }
    }

    pub(super) async fn recv_resume_parked(
        &self,
        from: Tid,
        mm: MmId,
        ticket: ResumeTicket,
        site: reverie::CallbackSignalSite,
    ) -> GlobalResponse {
        let tid = DetTid::from_raw(from.as_raw());
        let response = Ivar::new();
        let prepared = {
            let mut sched = self.lock_rpc_scheduler(false).await;
            match sched.parked_resume_resources(ticket, tid, mm, site) {
                Ok(resources) => {
                    let posted = sched.post_control(
                        tid,
                        mm,
                        ControlIntent::Resume {
                            ticket,
                            site,
                            response: response.clone(),
                        },
                    );
                    posted.map(|()| resources)
                }
                Err(error) => Err(error),
            }
        };
        let (pid, resources) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.sched.lock().unwrap().fail_parked(tid, error);
                return GlobalResponse::ThreadExited;
            }
        };
        let answer = response.await;
        match self
            .finish_resource_response(from, pid, resources, Some(mm), answer)
            .await
            .0
        {
            SchedulerRpcResult::Continue(reply) => GlobalResponse::ResumeParkedRequest(reply),
            SchedulerRpcResult::ThreadExited => GlobalResponse::ThreadExited,
        }
    }

    pub(super) async fn recv_signal_dequeued(
        &self,
        tid: DetTid,
        mm: MmId,
        guest_time: DetTime,
        pid: DetPid,
        identity: reverie::SignalTaskIdentity,
        dequeue: reverie::SignalDequeue,
    ) -> (Option<LogicalTime>, GlobalResponse) {
        let mut sched = self.lock_rpc_scheduler(true).await;
        let validation = sched
            .real_timers
            .validate_effect(pid, tid, mm, identity, dequeue.process);
        let result = validation.and_then(|live| {
            if live && !sched.backend_failed() {
                self.global_time.lock().unwrap().update_global_time(
                    tid,
                    guest_time.as_nanos(),
                    guest_time.inherited_nanos(),
                );
            }
            let now = self.global_time.lock().unwrap().as_nanos();
            sched.apply_signal_dequeue(pid, dequeue, now)
        });
        if let Err(error) = result {
            sched.fail_parked(tid, ProtocolFailure::Timer(error));
        }
        let terminal = sched.backend_failed() || matches!(result, Ok(DequeueAck::Retired { .. }));
        (
            None,
            GlobalResponse::SignalDequeued {
                ack: result,
                terminal,
            },
        )
    }
}

pub(crate) async fn signal_dequeued<G, T>(
    guest: &mut G,
    dequeue: reverie::SignalDequeue,
) -> Result<(), Errno>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !guest.config().kvm_shared_dequeue_timers {
        return Ok(());
    }
    let current_identity = guest.signal_task_identity();
    let state = guest.thread_state();
    let identity = state.signal_task_identity.ok_or(Errno::EIO)?;
    if current_identity.is_some_and(|current| current != identity)
        || identity.process != dequeue.process
    {
        return Err(Errno::EIO);
    }
    let request = GlobalRequest::SignalDequeued {
        detpid: state.detpid.ok_or(Errno::EIO)?,
        identity,
        dequeue,
    };
    // This is consuming notification, not an ordinary request that can be
    // replaced by ThreadExited/tail injection after irreversible removal.
    let (_, response) = guest
        .send_rpc((state.thread_logical_time.clone(), state.mm_id, request))
        .await;
    match response {
        GlobalResponse::SignalDequeued {
            ack: Ok(DequeueAck::Applied { sequence }),
            terminal: false,
        } if sequence == dequeue.sequence => Ok(()),
        _ => Err(Errno::EIO),
    }
}

pub(crate) async fn terminate_protocol<G, T>(guest: &mut G, failure: ProtocolFailure) -> !
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let state = guest.thread_state();
    let _ = guest
        .send_rpc((
            state.thread_logical_time.clone(),
            state.mm_id,
            GlobalRequest::ParkedProtocolFailure(failure),
        ))
        .await;
    if let Some(context) = guest.parked_signal_failure_context() {
        let _ = guest.cancel_parked_signal(context).await;
    }
    // Existing backend supervision consumes the failure. No guest errno,
    // normal exit or retry can erase an irreversible publication/removal.
    futures::future::pending().await
}

pub(crate) async fn parked_wait_request<G, T>(
    guest: &mut G,
    resources: Resources,
    policy: ParkedWaitPolicy,
) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if !guest.config().kvm_shared_dequeue_timers {
        return resource_request(guest, resources).await;
    }
    let Some(site) = guest.parked_signal_site() else {
        // An unsupported site remains ordinary; a due KVM timer refuses
        // through typed supervision before any host signal can be sent.
        return resource_request(guest, resources).await;
    };
    capable_resource_request(
        guest,
        resources,
        ControlCapability::ParkedWait { policy, site },
    )
    .await
}

pub(super) async fn capable_resource_request<G, T>(
    guest: &mut G,
    resources: Resources,
    capability: ControlCapability,
) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    capable_resource_request_with_write(guest, resources, capability, None).await
}

/// Only the original scalar write may request this capability. Descriptor
/// numbers and Detcore resource names alone do not prove capture: the backend
/// must authenticate its actual alias and the exact original callback.
pub(crate) async fn captured_write_request<G, T>(
    guest: &mut G,
    resources: Resources,
    call: reverie::syscalls::Write,
) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().kvm_shared_dequeue_timers
        && guest.config().sequentialize_threads
        && guest.signal_observation_lease().is_none()
        && resources.resources.len() == 1
        && resources.resources.iter().all(|(resource, permission)| {
            matches!(
                resource,
                ResourceID::Device(
                    crate::resources::Device::ContainerStdout
                        | crate::resources::Device::ContainerStderr
                )
            ) && *permission == Permission::W
        })
        && let Some(site) = guest.captured_write_signal_site(call)
    {
        return capable_resource_request_with_write(
            guest,
            resources,
            ControlCapability::CapturedWrite { site },
            Some(call),
        )
        .await;
    }
    resource_request(guest, resources).await
}

async fn capable_resource_request_with_write<G, T>(
    guest: &mut G,
    resources: Resources,
    capability: ControlCapability,
    captured_write: Option<reverie::syscalls::Write>,
) -> ResumeStatus
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let pid = guest.thread_state().detpid.expect("registered process");
    let response = send_and_update_time(
        guest,
        GlobalRequest::ParkedRequest(resources, pid, capability),
    )
    .await
    .1;
    let mut reply = match response {
        GlobalResponse::ParkedRequest(reply) => reply,
        _ => terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await,
    };
    loop {
        let control = match reply {
            ResourceReply::Grant(status) => return status,
            ResourceReply::PublishAlarm(control) => *control,
        };
        if guest.parked_signal_site() != Some(control.site)
            || control.request.dettid != guest.thread_state().dettid
            || control.request.mm != guest.thread_state().mm_id
            || captured_write
                .is_some_and(|call| guest.captured_write_signal_site(call) != Some(control.site))
        {
            terminate_protocol(guest, ProtocolFailure::Identity).await;
        }
        let outcome = guest.queue_process_alarm_signal(control.event).await;
        let activation =
            match send_and_update_time(guest, GlobalRequest::AlarmPublicationAck(control, outcome))
                .await
                .1
            {
                GlobalResponse::AlarmPublicationAck(Ok(activation)) => activation,
                GlobalResponse::AlarmPublicationAck(Err(error)) => {
                    terminate_protocol(guest, error).await
                }
                _ => terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await,
            };
        let ticket = match activation {
            PublicationActivation::AwaitResume(ticket) => ticket,
            PublicationActivation::Observe { wait, lease } => {
                if captured_write.is_some() {
                    terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await;
                }
                let observation = match guest.observe_parked_signal(control.site, lease).await {
                    Ok(result) => result,
                    Err(error) => {
                        terminate_protocol(guest, ProtocolFailure::Observation(error)).await
                    }
                };
                let finish = match observation.stop {
                    reverie::SignalObservationStop::NoEligibleSignal => {
                        ObservationFinish::ResumeSameWait
                    }
                    reverie::SignalObservationStop::Caught(selection) => {
                        ObservationFinish::InterruptForCaught { selection }
                    }
                    reverie::SignalObservationStop::Fatal(selection) => {
                        ObservationFinish::Terminate { selection }
                    }
                };
                let ack = match send_and_update_time(
                    guest,
                    GlobalRequest::FinishParkedObservation {
                        wait,
                        lease,
                        site: control.site,
                        finish,
                    },
                )
                .await
                .1
                {
                    GlobalResponse::FinishParkedObservation(Ok(ack)) => ack,
                    GlobalResponse::FinishParkedObservation(Err(error)) => {
                        terminate_protocol(guest, error).await
                    }
                    _ => terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await,
                };
                match (ack, finish) {
                    (FinishAck::AwaitResume(ticket), ObservationFinish::ResumeSameWait) => ticket,
                    (FinishAck::Interrupted, ObservationFinish::InterruptForCaught { .. }) => {
                        return ResumeStatus::Signaled(None);
                    }
                    (FinishAck::Terminate, ObservationFinish::Terminate { selection }) => {
                        match guest.terminate_from_parked_signal(selection).await {
                            Ok(never) => match never {},
                            Err(error) => {
                                terminate_protocol(guest, ProtocolFailure::Observation(error)).await
                            }
                        }
                    }
                    _ => terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await,
                }
            }
        };
        let Some(current_site) = guest.parked_signal_site() else {
            terminate_protocol(guest, ProtocolFailure::Identity).await;
        };
        reply = match send_and_update_time(
            guest,
            GlobalRequest::ResumeParkedRequest {
                ticket,
                current_site,
            },
        )
        .await
        .1
        {
            GlobalResponse::ResumeParkedRequest(reply) => reply,
            _ => terminate_protocol(guest, ProtocolFailure::UnexpectedControl).await,
        };
    }
}

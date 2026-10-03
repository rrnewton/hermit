//! Original connect uses the existing native worker, Calls and provider Driver.
//! The Driver consumes early selection independently of every Guest future.
use accepted_controller::Effect;
use accepted_provider::Reply;
use accepted_provider::Request;

use super::*;
use crate::network_replay::NetworkReplayError;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Kind;
use crate::network_replay::original_connect::Pin;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub(crate) admission: Admission,
    pub(crate) returned: i64,
    pub(crate) pin: Option<Pin>,
    pub(crate) address: Option<Vec<u8>>,
    pub(crate) socket: Option<super::installation_observation::Checked>,
    pub(crate) read_copy: Option<super::original_read_copy::Capture>,
}

impl NativeCaptureRecovery {
    /// Use the same semantic-retirement/port publication boundary as existing
    /// capture recovery. The caller must already own a known no-acquisition or
    /// actual close; engine phase checks reject every unresolved alternative.
    pub(super) fn retire_original_lifetime(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        before_provider: bool,
    ) -> std::io::Result<()> {
        let retired = {
            let mut engine = self.engine.lock().unwrap();
            if before_provider {
                engine.abort_original_before_provider(owner, admission)
            } else {
                engine.finish_original_connect(owner, admission)
            }
            .map_err(std::io::Error::other)?;
            engine.take_lifetime_retired_ports().into_iter().collect()
        };
        (self.retire_ports)(retired);
        self.changed.notify_waiters();
        Ok(())
    }
}

impl RuntimeShared {
    fn publish_original_close_selection(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
        selected: &super::accepted_provider::OriginalSelection,
    ) -> std::io::Result<()> {
        // Re-read the actual backend-bound Arc after observing the physical
        // receipt. A Driver snapshot can precede Prepared; it has no authority
        // to substitute another task's metadata or infer missing preparation.
        let metadata = self
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .close_metadata
            .clone()
            .ok_or_else(|| {
                std::io::Error::other("close effect lacks actual pre-invocation metadata custody")
            })?;
        let mut local = metadata.lock().unwrap();
        publication
            .engine
            .lock()
            .unwrap()
            .publish_original_close_selection(owner, admission, selected, &mut local)
            .map_err(std::io::Error::other)
    }
    /// Runs only in the same capture worker, before any provider request exists.
    /// Both classification failure and consumed pre-submission cancellation use
    /// the existing ReleaseExecution and exact semantic/port retirement join.
    pub(super) fn retire_original_before_submission(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        let work = {
            let mut calls = self.native_streams.lock().unwrap();
            calls.abandon_original_before_provider(owner, admission.call)?;
            calls.prepare_release(owner, admission.call)?
        };
        let release = work.perform();
        self.native_streams
            .lock()
            .unwrap()
            .retain_release(owner, admission.call, release)?;
        publication.retire_original_lifetime(owner, admission, true)?;
        self.native_streams
            .lock()
            .unwrap()
            .finish_release(owner, admission.call)?;
        publication.changed.notify_waiters();
        Ok(())
    }
    /// Wait only for already-owned original Calls. Normal capture/effect
    /// admission is closed by the caller. The same Driver and exact retained
    /// retirement workers own every physical action; one original deadline
    /// covers this wait and all subsequent joins.
    pub(super) async fn finish_original_connects(
        self: &std::sync::Arc<Self>,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        let operation = async {
            loop {
                let Some((_, state)) = self
                    .native_streams
                    .lock()
                    .unwrap()
                    .originals()
                    .into_iter()
                    .next()
                else {
                    return Ok(());
                };
                let changed = state.publication.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self
                    .native_streams
                    .lock()
                    .unwrap()
                    .originals()
                    .iter()
                    .any(|(_, current)| current.admission == state.admission)
                {
                    continue;
                }
                let controller = match self.controller.lock().unwrap().clone() {
                    Some(Ok(controller)) => controller,
                    Some(Err(error)) => return Err(std::io::Error::other(error)),
                    None => {
                        return Err(std::io::Error::other(
                            "owned original has no provider controller",
                        ));
                    }
                };
                // The Driver owns progress; a sticky semantic failure does not
                // skip its exact physical cleanup. Actual transport failure is
                // still an error and cannot prove any missing retirement.
                tokio::select! {_=changed=>{},error=controller.failure()=>return Err(error)}
            }
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .map_err(|_| {
                std::io::Error::other(
                    "owned original retirement exceeded original terminal deadline",
                )
            })?
    }
    fn progress_original_read_copy(
        &self,
        controller: &accepted_controller::Controller,
        owner: NetworkStreamOwner,
        state: &mut super::native_peer::OriginalConnect,
    ) -> std::io::Result<()> {
        if state.admission.arguments.kind != Kind::Read
            || state.canceled
            || state.selection.is_none() && state.terminal.is_none()
        {
            return Ok(());
        }
        let call = state.admission.call;
        let (prepared, command) = state
            .prepared
            .ok_or_else(|| std::io::Error::other("Read prefix preparation missing"))?;
        // Bind the actual selected OR final-wait Call before authority joins.
        // No engine/custody lock crosses Controller validation or native work.
        let mut received = state
            .publication
            .engine
            .lock()
            .unwrap()
            .bind_original_read_copy(owner, &state.admission, command, &state.copy_custody)
            .map_err(std::io::Error::other)?;
        if !state.copy_custody.has_wire_authority()?
            && !state.copy_custody.has_empty_terminal_authority()?
        {
            let selected = if state.selection.is_some() {
                state.selection_request
            } else {
                state.completion_request
            }
            .ok_or_else(|| std::io::Error::other("Read wire selection receipt missing"))?;
            let prepared_wire = state.copy_custody.take_preparation()?;
            if state.selection.is_none()
                && state
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| terminal.fd_call_present == 0)
            {
                let authority =
                    controller.empty_copy_terminal_authority(prepared_wire, selected)?;
                state
                    .copy_custody
                    .prepare_empty_terminal(authority, state.terminal.as_ref().unwrap())?;
            } else {
                let wire = controller.bind_copy_authority(prepared_wire, selected)?;
                state.copy_custody.prepare(wire)?;
            }
        }
        let mut next = state.copy_custody.len()? as u64;
        for &(first, sequence) in &state.copy_requests {
            if first < next {
                continue;
            }
            if state.copy_end.is_some() {
                break;
            }
            if first != next {
                return Err(std::io::Error::other("Read prefix request gap"));
            }
            let Some(reply) = controller.retained_response(sequence)? else {
                break;
            };
            let Reply::OriginalReadCopy(chunk) = reply else {
                return Err(std::io::Error::other("Read prefix response changed kind"));
            };
            if chunk.prepared != prepared
                || chunk.first != next
                || chunk.records.len() > super::original_read_copy::RECORDS_PER_REPLY
                || chunk.records.is_empty() && chunk.end.is_none()
            {
                return Err(std::io::Error::other(
                    "Read prefix changed original preparation/position",
                ));
            }
            next = next
                .checked_add(chunk.records.len() as u64)
                .ok_or_else(|| std::io::Error::other("Read prefix record count overflow"))?;
            let chunk_first = chunk.first;
            state.copy_end = chunk.end;
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, call)?
                .copy_end = state.copy_end;
            // Preserve the raw reply before interpreting it; a refusal does
            // not erase its same-Call physical/terminal custody. The retained
            // parser visits only new records, before another request is issued.
            let parsed = if state.copy_custody.has_empty_terminal_authority()? {
                let terminal = state.terminal.as_ref().ok_or_else(|| {
                    std::io::Error::other("empty Read terminal lost its retained receipt")
                })?;
                state.copy_custody.append_empty_terminal(
                    terminal,
                    chunk_first,
                    chunk.records,
                    state.copy_end,
                )
            } else {
                let selection = state
                    .selection
                    .as_ref()
                    .or_else(|| {
                        state
                            .terminal
                            .as_ref()
                            .map(|terminal| &terminal.original.selection)
                    })
                    .ok_or_else(|| std::io::Error::other("Read prefix lost original selection"))?;
                state
                    .copy_custody
                    .append(selection, chunk_first, chunk.records, state.copy_end)
            };
            // No custody mutex is held across the engine lock. The valid
            // completed prefix is retained even when this suffix is malformed.
            if let Some(delta) = state.copy_custody.completed_since(received)? {
                received = state
                    .publication
                    .engine
                    .lock()
                    .unwrap()
                    .retain_original_read_copy(owner, &state.admission, delta)
                    .map_err(std::io::Error::other)?;
            }
            // Existing maintenance owns this progress independently of the
            // still-blocked Guest. A syscall result is not manufactured here.
            state.publication.changed.notify_waiters();
            parsed?;
        }
        if state.copy_end.is_none()
            && state
                .copy_requests
                .last()
                .is_none_or(|(first, _)| *first < next)
        {
            let sequence = controller.prepare(
                Effect::ReadOriginalCopy(call, next),
                owner,
                &Request::ReadOriginalCopy {
                    call: call.native_command_call(),
                    command,
                    prepared,
                    first: next,
                },
                || Ok(vec![]),
            )?;
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, call)?
                .copy_requests
                .push((next, sequence));
            state.copy_requests.push((next, sequence));
        }
        Ok(())
    }
    fn progress_original_connects(
        self: &std::sync::Arc<Self>,
        controller: &accepted_controller::Controller,
    ) -> std::io::Result<()> {
        let calls = self.native_streams.lock().unwrap().originals();
        for (owner, mut state) in calls {
            // A lost final Guest callback is also finite. The physical close
            // worker owns completion, and only the actual consumed marker
            // authorizes the Driver to retire its unconsumed Call afterward.
            if state.close_queued {
                // An already-owned history worker must retain its raw outcome
                // before the same Call can disappear, including final wait.
                if state.control_history_started && state.control_history.is_none() {
                    continue;
                }
                let engine = state.publication.engine.lock().unwrap();
                let (canceled, consumed) =
                    match engine.original_connect_cancellation(owner, &state.admission) {
                        Ok(value) => value,
                        Err(NetworkReplayError::UnknownStreamCall(_)) => continue, // consumer retirement may be between its two exact removals
                        Err(error) => return Err(std::io::Error::other(error)),
                    };
                let terminal = engine
                    .original_connect_task_terminal(owner, &state.admission)
                    .map_err(std::io::Error::other)?;
                if (consumed || terminal)
                    && engine
                        .original_connect_close_confirmed(owner, &state.admission)
                        .map_err(std::io::Error::other)?
                    && self
                        .native_streams
                        .lock()
                        .unwrap()
                        .original_closed(owner, state.admission.call)?
                {
                    let pending = engine
                        .terminal_allocator_pending(owner, &state.admission)
                        .map_err(std::io::Error::other)?;
                    drop(engine);
                    if pending {
                        if !state.terminal_allocator_started {
                            self.native_streams
                                .lock()
                                .unwrap()
                                .original(owner, state.admission.call)?
                                .terminal_allocator_started = true;
                            let shared = self.clone();
                            let admitted = state.admission.clone();
                            let publication = state.publication.clone();
                            let executor = state.executor.clone();
                            let _ = self.start_original_retirement_worker(
                                owner,
                                admitted.call,
                                state.executor.clone(),
                                move || {
                                    let result = executor.block_on(
                                        shared.reconcile_terminal_allocator(owner, &admitted),
                                    );
                                    if let Err(error) = &result {
                                        shared
                                            .native_terminal_failure
                                            .lock()
                                            .unwrap()
                                            .get_or_insert(error.to_string());
                                    }
                                    publication.changed.notify_waiters();
                                    result
                                },
                            )?;
                        }
                        continue;
                    }
                    state
                        .publication
                        .retire_original_lifetime(owner, &state.admission, false)?;
                    self.native_streams
                        .lock()
                        .unwrap()
                        .finish_release(owner, state.admission.call)?;
                    if !canceled {
                        self.native_terminal_failure.lock().unwrap().get_or_insert(
                            "original Connect consumer exited before semantic capture".into(),
                        );
                    }
                    state.publication.changed.notify_waiters();
                }
                continue;
            }
            let admission_value = state.admission.clone();
            let admission = &admission_value;
            let call = admission.call;
            if state.prepared.is_none() {
                let Some(request) = state.prepare_request else {
                    continue;
                };
                let Some(reply) = controller.retained_response(request)? else {
                    continue;
                };
                let Reply::Prepared(observed) = reply else {
                    return Err(std::io::Error::other(
                        "original preparation reply changed operation",
                    ));
                };
                if observed.status.returned != 0 || observed.raw == 0 {
                    return Err(std::io::Error::other(format!(
                        "original preparation unresolved: {observed:?}"
                    )));
                }
                if admission.arguments.kind == Kind::Read {
                    let wire =
                        controller.prepared_copy_authority(owner, call, request, observed.raw)?;
                    state.copy_custody.retain_preparation(wire)?;
                }
                let query = controller.prepare(
                    Effect::AwaitOriginalSelection(call),
                    owner,
                    &Request::AwaitOriginalSelection {
                        call: call.native_command_call(),
                        command: observed.raw,
                        prepared_request: request,
                    },
                    || Ok(vec![]),
                )?;
                // The controller already retains the actual provider response.
                // Publish readiness in Calls only after engine admission agrees,
                // so a spurious wake cannot inject before that admission exists.
                state
                    .publication
                    .engine
                    .lock()
                    .unwrap()
                    .original_call_prepared(owner, admission, state.pin, observed.raw)
                    .map_err(std::io::Error::other)?;
                self.native_streams.lock().unwrap().original_prepared(
                    owner,
                    admission,
                    request,
                    observed.raw,
                    query,
                )?;
                state.prepared = Some((request, observed.raw));
                state.selection_request = Some(query);
                state.publication.changed.notify_waiters();
            }
            let (cancel_requested, _) = state
                .publication
                .engine
                .lock()
                .unwrap()
                .original_connect_cancellation(owner, admission)
                .map_err(std::io::Error::other)?;
            let terminal = state
                .publication
                .engine
                .lock()
                .unwrap()
                .original_connect_task_terminal(owner, admission)
                .map_err(std::io::Error::other)?;
            if terminal && (state.completion_request.is_none() || state.terminating) {
                if state.completion_request.is_none() {
                    let sequence = controller.prepare(
                        Effect::TerminateOriginalConnect(call),
                        owner,
                        &Request::TerminateOriginalConnect {
                            call: call.native_command_call(),
                            command: state.prepared.unwrap().1,
                            prepared_request: state.prepared.unwrap().0,
                            selected_request: state.selection_request.unwrap(),
                            failed_request: state
                                .failed_collection
                                .as_ref()
                                .map(|(sequence, _)| *sequence),
                        },
                        || Ok(vec![]),
                    )?;
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .completion_request = Some(sequence);
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .terminating = true;
                    state.completion_request = Some(sequence);
                    state.terminating = true;
                }
                if state.terminal.is_none() {
                    let Some(reply) =
                        controller.retained_response(state.completion_request.unwrap())?
                    else {
                        continue;
                    };
                    let Reply::OriginalTerminated(observation) = reply else {
                        return Err(std::io::Error::other(
                            "dead original returned another effect",
                        ));
                    };
                    if observation.status.returned != 0
                        || observation.status.errno.is_some()
                        || observation.raw.command.command != state.prepared.unwrap().1
                        || observation.raw.call != call.native_command_call()
                        || observation.raw.task_absent != 1
                    {
                        return Err(std::io::Error::other(format!(
                            "dead original physical retirement unresolved: {observation:?}"
                        )));
                    }
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .terminal = Some(observation.raw.clone());
                    state.terminal = Some(observation.raw);
                    state.publication.changed.notify_waiters();
                }
            } else if state.failed_collection.is_some() {
                continue; // Run is RED; only actual final wait can authorize retirement.
            } else if cancel_requested {
                if state.completion_request.is_none() {
                    let sequence = controller.prepare(
                        Effect::CancelOriginalConnect(call),
                        owner,
                        &Request::CancelOriginalConnect {
                            call: call.native_command_call(),
                            command: state.prepared.unwrap().1,
                            prepared_request: state.prepared.unwrap().0,
                            selected_request: state.selection_request.unwrap(),
                        },
                        || Ok(vec![]),
                    )?;
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .completion_request = Some(sequence);
                    state.completion_request = Some(sequence);
                }
                if !state.canceled {
                    let Some(reply) =
                        controller.retained_response(state.completion_request.unwrap())?
                    else {
                        continue;
                    };
                    if !matches!(reply,Reply::OriginalCanceled {command,status}
                        if command==state.prepared.unwrap().1 && status.returned==0 && status.errno.is_none())
                    {
                        return Err(std::io::Error::other(
                            "known-uninvoked original disarm was not positively observed",
                        ));
                    }
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_connect_disarmed(owner, admission, state.prepared.unwrap().1)
                        .map_err(std::io::Error::other)?;
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .canceled = true;
                    state.canceled = true;
                    state.publication.changed.notify_waiters();
                }
            } else {
                if admission.arguments.kind == Kind::EpollCtl
                    && !self.progress_original_epoll_ctl(controller, owner, &mut state)?
                {
                    continue;
                }
                if state.selection.is_none()
                    && let Some(reply) =
                        controller.retained_response(state.selection_request.unwrap())?
                    {
                        let Reply::OriginalSelection(observed) = reply else {
                            return Err(std::io::Error::other(
                                "original early query changed operation",
                            ));
                        };
                        if observed.status.returned != 0 {
                            return Err(std::io::Error::other(format!(
                                "original selection unresolved: {observed:?}"
                            )));
                        }
                        let raw = observed.raw;
                        // Retain the physical observation before releasing either
                        // exclusion. No lock spans another owner's lock or IO.
                        self.native_streams.lock().unwrap().original_selected(
                            owner,
                            admission,
                            raw.clone(),
                        )?;
                        if admission.arguments.kind == Kind::Close {
                            self.publish_original_close_selection(
                                owner,
                                admission,
                                &state.publication,
                                &raw,
                            )?;
                        } else {
                            state
                                .publication
                                .engine
                                .lock()
                                .unwrap()
                                .original_connect_selected(
                                    owner,
                                    admission,
                                    raw.command,
                                    (raw.provider, raw.task, raw.task_start, raw.table, raw.file),
                                )
                                .map_err(std::io::Error::other)?;
                        }
                        state.selection = Some(raw);
                        state.publication.changed.notify_waiters();
                    }
                self.progress_original_read_copy(controller, owner, &mut state)?;
                let native = state
                    .publication
                    .engine
                    .lock()
                    .unwrap()
                    .original_connect_result(owner, admission)
                    .map_err(std::io::Error::other)?;
                if state.completion_request.is_none() {
                    let Some(_) = native else {
                        continue;
                    };
                    let (prepared_request, command) = state.prepared.unwrap();
                    let sequence = controller.prepare(
                        Effect::CollectOriginalConnect(call),
                        owner,
                        &Request::CollectOriginalConnect {
                            kind: admission.arguments.kind,
                            call: call.native_command_call(),
                            command,
                            prepared_request,
                        },
                        || Ok(vec![]),
                    )?;
                    self.native_streams
                        .lock()
                        .unwrap()
                        .original(owner, call)?
                        .completion_request = Some(sequence);
                    state.completion_request = Some(sequence);
                }
                if state.completion.is_none() {
                    let Some(reply) =
                        controller.retained_response(state.completion_request.unwrap())?
                    else {
                        continue;
                    };
                    let Reply::OriginalEffect(observed) = reply else {
                        return Err(std::io::Error::other(
                            "original final query changed operation",
                        ));
                    };
                    if observed.status.returned != 0 {
                        let failed = (state.completion_request.unwrap(), observed);
                        let mut calls = self.native_streams.lock().unwrap();
                        let retained = calls.original(owner, call)?;
                        retained.failed_collection = Some(failed);
                        retained.completion_request = None;
                        drop(calls);
                        self.native_terminal_failure.lock().unwrap().get_or_insert(
                            "original native completion UNKNOWN; failed collection retained until actual task terminal".into());
                        state.publication.changed.notify_waiters();
                        continue;
                    }
                    let returned = native
                        .ok_or_else(|| std::io::Error::other("original backend result missing"))?;
                    self.native_streams.lock().unwrap().original_completed(
                        owner,
                        admission,
                        observed.raw.clone(),
                        returned,
                    )?;
                    state.completion = Some(observed.raw);
                }
            } // invoked completion and known-uninvoked disarm are distinct paths
            if admission.arguments.kind.allocator()
                && let Some(terminal) = &state.terminal {
                    if terminal.fd_call_present != 1 || terminal.original.complete != 1 {
                        // Actual task death alone cannot certify either success,
                        // failure or no invocation. Keep the exact raw custody.
                        continue;
                    }
                    let selected = terminal.original.selection.clone();
                    self.native_streams.lock().unwrap().original_selected(
                        owner,
                        admission,
                        selected.clone(),
                    )?;
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_terminal_allocator_completed(owner, admission, &terminal.original)
                        .map_err(std::io::Error::other)?;
                    state.selection = Some(selected);
                }
            if admission.arguments.kind == Kind::Read && !state.canceled {
                self.progress_original_read_copy(controller, owner, &mut state)?;
                let Some(end) = state.copy_end else { continue };
                if let Some(effect) = state.completion.as_ref() {
                    if end
                        != (super::original_read_copy::End::OriginalExit {
                            protocol: effect.read_copy.is_some_and(|m| m.present == 1),
                        })
                    {
                        return Err(std::io::Error::other(
                            "completed Read prefix lost actual EXIT",
                        ));
                    }
                    if state.copy_capture.is_none() {
                        let capture = state.copy_custody.collect(effect)?;
                        self.native_streams
                            .lock()
                            .unwrap()
                            .original(owner, call)?
                            .copy_capture = Some(capture.clone());
                        state.copy_capture = Some(capture);
                    }
                } else if let Some(terminal) = state.terminal.as_ref() {
                    state.copy_custody.validate_terminal(terminal, end)?;
                }
            }
            if admission.arguments.kind == Kind::Close
                && let Some(terminal) = &state.terminal {
                    if terminal.fd_call_present == 1 && terminal.original.selection.ready == 1 {
                        if terminal.original.problem != 0 {
                            return Err(std::io::Error::other(
                                "terminal close selection has a provider problem",
                            ));
                        }
                        let selected = terminal.original.selection.clone();
                        self.native_streams.lock().unwrap().original_selected(
                            owner,
                            admission,
                            selected.clone(),
                        )?;
                        self.publish_original_close_selection(
                            owner,
                            admission,
                            &state.publication,
                            &selected,
                        )?;
                        state.selection = Some(selected);
                        state.publication.changed.notify_waiters();
                    } else if state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_close_terminal_publication_pending(owner, admission)
                        .map_err(std::io::Error::other)?
                    {
                        // Final wait already published the shared backend
                        // failure. Missing effects cannot authorize a live peer
                        // to observe/reuse this table. Normal consuming owner
                        // cleanup, not a fabricated removal, closes this fence.
                        continue;
                    }
                }
            if state.retirement_request.is_none() {
                let sequence = controller.prepare(
                    Effect::RetireOriginalConnect(call),
                    owner,
                    &Request::RetireOriginalConnect {
                        failed_request: state
                            .failed_collection
                            .as_ref()
                            .map(|(sequence, _)| *sequence),
                        call: call.native_command_call(),
                        prepared: state.prepared.unwrap().0,
                        selected: state.selection_request.unwrap(),
                        completed: state.completion_request.unwrap(),
                    },
                    || Ok(vec![]),
                )?;
                self.native_streams
                    .lock()
                    .unwrap()
                    .original(owner, call)?
                    .retirement_request = Some(sequence);
                state.retirement_request = Some(sequence);
            }
            if !state.retired {
                let Some(reply) =
                    controller.retained_response(state.retirement_request.unwrap())?
                else {
                    continue;
                };
                if !matches!(reply, Reply::Retired) {
                    return Err(std::io::Error::other(
                        "original retirement did not acknowledge exact custody",
                    ));
                }
                controller.retire_original(
                    owner,
                    call,
                    [
                        state.prepared.unwrap().0,
                        state.selection_request.unwrap(),
                        state.completion_request.unwrap(),
                        state.retirement_request.unwrap(),
                    ],
                    state.canceled,
                    state.terminal.is_some(),
                    state
                        .failed_collection
                        .as_ref()
                        .map(|(sequence, _)| *sequence),
                )?;
                if state.terminal.is_some() {
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_connect_dead_retired(owner, admission, state.prepared.unwrap().1)
                        .map_err(std::io::Error::other)?;
                    state.publication.changed.notify_waiters();
                } else if state.canceled {
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_connect_cancel_retired(owner, admission)
                        .map_err(std::io::Error::other)?;
                } else {
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .original_connect_provider_retired(
                            owner,
                            admission,
                            i64::from(state.completion.as_ref().unwrap().original.returned),
                        )
                        .map_err(std::io::Error::other)?;
                }
                self.native_streams
                    .lock()
                    .unwrap()
                    .original(owner, call)?
                    .retired = true;
                state.retired = true;
            }
            if !state.close_queued {
                let shared = self.clone();
                let admission = admission.clone();
                let publication = state.publication.clone();
                // Same registry, same captured Tokio executor. Calling the
                // thread-local spawn_blocking from this native Driver would be
                // invalid; no second executor/worker owner is created here.
                // Latch before spawning: this worker may complete and the
                // consumer may retire Calls before start_native_worker returns.
                self.native_streams
                    .lock()
                    .unwrap()
                    .original(owner, call)?
                    .close_queued = true;
                let operation = move || {
                        let result: std::io::Result<()> = (|| {
                            let work = shared
                                .native_streams
                                .lock()
                                .unwrap()
                                .prepare_release(owner, admission.call)?;
                            let release = work.perform();
                            shared.native_streams.lock().unwrap().retain_release(
                                owner,
                                admission.call,
                                release,
                            )?;
                            let mut engine = publication.engine.lock().unwrap();
                            engine
                                .original_connect_pin_released(owner, &admission)
                                .map_err(std::io::Error::other)?;
                            Ok(())
                        })();
                        if let Err(error) = &result {
                            shared
                                .native_terminal_failure
                                .lock()
                                .unwrap()
                                .get_or_insert(error.to_string());
                        }
                        publication.changed.notify_waiters();
                        result
                    };
                if let Some(origin) = state.shared_send.clone() {
                    self.start_shared_send_close(origin, state.canceled || state.terminal.is_some() || state.terminating || state.failed_collection.is_some(), state.executor.clone(), operation)?;
                } else {
                    let _ = self.start_original_retirement_worker(owner, call, state.executor.clone(), operation)?;
                }
            }
        }
        Ok(())
    }
    pub(super) fn retain_original_connects(
        self: &std::sync::Arc<Self>,
        controller: &accepted_controller::Controller,
    ) -> std::io::Result<()> {
        let result = self.progress_original_connects(controller);
        if let Err(error) = &result {
            self.native_terminal_failure
                .lock()
                .unwrap()
                .get_or_insert(error.to_string());
            for (_, state) in self.native_streams.lock().unwrap().originals() {
                state.publication.changed.notify_waiters();
            }
        }
        result
    }
}

impl NetworkRuntimeResources {
    pub(crate) fn bind_original_close_metadata(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        metadata: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    ) -> std::io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .bind_original_close_metadata(owner, admission, metadata)
    }
    pub(crate) fn bind_original_installation_metadata(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        metadata: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    ) -> std::io::Result<()> {
        let (provider, task, start, table) = self
            .shared
            .physical
            .lock()
            .unwrap()
            .installation_identity(owner)?;
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .bind_original_installation_owner(
                owner,
                admission,
                super::original_installation::Owner {
                    owner,
                    metadata,
                    files: admission.arguments.files,
                    provider,
                    task,
                    start,
                    table,
                },
            )
    }
    pub(crate) async fn prepare_original_connect(
        &self,
        owner: NetworkStreamOwner,
        admission: Admission,
        task: OwnedFd,
        publication: NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        let target = std::sync::Arc::new(task);
        let setup = (|| {
            let userns = if admission.arguments.kind == Kind::Openat {
                Some(super::openat_observation::same_user_namespace(
                    target.as_fd(),
                )?)
            } else {
                None
            };
            let needs_netns = admission.arguments.kind == Kind::Socket
                && matches!(admission.arguments.fd, libc::AF_INET | libc::AF_INET6)
                && (admission.arguments.address as u32 as i32)
                    & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                    == libc::SOCK_STREAM
                && matches!(admission.arguments.length, 0 | libc::IPPROTO_TCP)
                && publication.engine.lock().unwrap().shadow_mode();
            let netns = if needs_netns {
                // Installed pidfd UAPI _IO(PIDFS_IOCTL_MAGIC, 4). The pidfd,
                // not a potentially reused numeric TID, selects the namespace.
                const PIDFD_GET_NET_NAMESPACE: libc::c_ulong = 0xff04;
                let raw = unsafe { libc::ioctl(target.as_raw_fd(), PIDFD_GET_NET_NAMESPACE, 0) };
                if raw < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Some(std::sync::Arc::new(unsafe { OwnedFd::from_raw_fd(raw) }))
            } else {
                None
            };
            Ok((
                self.accepted_controller()?,
                tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?,
                userns,
                netns,
            ))
        })();
        let (controller, executor, userns, netns) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                publication.retire_original_lifetime(owner, &admission, true)?;
                publication.changed.notify_waiters();
                return Err(error);
            }
        };
        let shared = self.shared.clone();
        let admitted = admission.clone();
        let authority = publication.clone();
        let capture_target = target.clone();
        let retained_target = target.clone();
        let launched = self.start_native_worker(move || {
            // Close must not duplicate the file: an extra reference would
            // defer final fput/socket release and change flush/linger behavior.
            let pin = if !matches!(admitted.arguments.kind, Kind::Connect | Kind::Sendto) {
                None
            } else {
                match capture_socket(&capture_target, admitted.arguments.fd) {
                    Ok(pin) => Some(pin),
                    Err(error)
                        if error.raw_os_error() == Some(libc::EBADF)
                            && admitted.arguments.binding.is_none() =>
                    {
                        None
                    }
                    Err(error) => {
                        // A failed pidfd_getfd installs no descriptor. No provider
                        // preparation or guest invocation has been possible yet.
                        authority.retire_original_lifetime(owner, &admitted, true)?;
                        authority.changed.notify_waiters();
                        return Err(error);
                    }
                }
            };
            shared.native_streams.lock().unwrap().capture_original(
                owner,
                admitted.clone(),
                pin,
                executor,
                authority.clone(),
            )?;
            if admitted.arguments.kind.allocator() {
                let mut calls = shared.native_streams.lock().unwrap();
                let state = calls.original(owner, admitted.call)?;
                state.allocation_task = Some(retained_target);
                state.allocation_userns = userns;
                state.allocation_netns = netns;
            }
            let held = shared
                .native_streams
                .lock()
                .unwrap()
                .original_reference(owner, admitted.call)?;
            let classified = if !matches!(admitted.arguments.kind, Kind::Connect | Kind::Sendto) {
                if held.is_some() {
                    return Err(std::io::Error::other("close acquired a physical file pin"));
                }
                Ok(None)
            } else {
                if admitted.arguments.kind == Kind::Sendto {
                    held.as_ref().ok_or_else(|| std::io::Error::other("Sendto has no original pin"))
                        .and_then(|pin| super::original_send::classify(pin)).map(Some)
                } else {
                    held.as_ref().map_or(Ok(Pin::Empty), |pin| native_peer::classify_original(pin)).map(Some)
                }
            };
            drop(held);
            let result = classified.and_then(|pin| {
                if let Some(pin) = pin {
                    shared.native_streams.lock().unwrap().original_classified(
                        owner,
                        admitted.call,
                        pin,
                    )?;
                }
                Ok(())
            });
            if let Err(error) = result {
                shared.retire_original_before_submission(owner, &admitted, &authority)?;
                return Err(error);
            }
            // Consume cancellation in the same physical owner; there is no
            // callback-dependent gap between pin capture and provider submission.
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
            // Setting this latch and checking cancellation are one engine cut.
            // Cancellation after it is settled by actual provider disarm below.
            let a = &admitted.arguments;
            let sequence = controller.prepare(
                Effect::PrepareOriginalConnect(admitted.call),
                owner,
                &Request::PrepareOriginalConnect {
                    kind: admitted.arguments.kind,
                    call: admitted.call.native_command_call(),
                    mm: owner.mm.generation(),
                    fd: a.fd,
                    address: a.address,
                    length: a.length,
                    original_count: a.original_count,
                },
                || Ok(vec![target.as_fd().try_clone_to_owned()?]),
            )?;
            shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admitted.call)?
                .prepare_request = Some(sequence);
            authority.changed.notify_waiters();
            Ok(())
        });
        let (worker, receive) = match launched {
            Ok(worker) => worker,
            Err(error) => {
                // start_native_worker refused before spawning; no capture or
                // provider submission ran. This is an exact no-effect result.
                publication.retire_original_lifetime(owner, &admission, true)?;
                publication.changed.notify_waiters();
                return Err(error);
            }
        };
        let result = receive.await;
        self.shared.join_native_worker(&worker).await?;
        result.map_err(std::io::Error::other)??;
        self.wait_original(owner, &admission, &publication, false)
            .await
            .map(|_| ())
    }
    pub(super) async fn wait_original(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
        closed: bool,
    ) -> std::io::Result<native_peer::OriginalConnect> {
        let controller = self.accepted_controller()?;
        loop {
            let changed = publication.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(error) = self.shared.native_terminal_failure.lock().unwrap().clone() {
                return Err(std::io::Error::other(error));
            }
            let confirmed = !closed
                || publication
                    .engine
                    .lock()
                    .unwrap()
                    .original_connect_close_confirmed(owner, admission)
                    .map_err(std::io::Error::other)?;
            let result = {
                let mut calls = self.shared.native_streams.lock().unwrap();
                let ready =
                    !closed || (confirmed && calls.original_closed(owner, admission.call)?);
                let call = calls.original(owner, admission.call)?;
                if call.admission != *admission {
                    return Err(std::io::Error::other(
                        "original wait changed admitted invocation",
                    ));
                }
                (call.prepared.is_some() && ready).then(|| call.clone())
            };
            if let Some(result) = result {
                return Ok(result);
            }
            tokio::select! {
                _=changed=>{},
                error=controller.failure()=>return Err(error),
            }
        }
    }
    pub(crate) async fn original_connect_outcome(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
    ) -> std::io::Result<Outcome> {
        let state = self
            .wait_original(owner, admission, publication, true)
            .await?;
        let result = &state
            .completion
            .as_ref()
            .ok_or_else(|| std::io::Error::other("original closed without completion"))?
            .original;
        let address = if admission.arguments.kind == Kind::Connect && result.security_returned == 1 {
            let length = usize::try_from(result.selection.address_length)
                .map_err(|_| std::io::Error::other("negative captured sockaddr length"))?;
            Some(
                result
                    .address
                    .get(..length)
                    .ok_or_else(|| {
                        std::io::Error::other("captured sockaddr exceeds kernel storage")
                    })?
                    .to_vec(),
            )
        } else {
            None
        };
        Ok(Outcome {
            admission: admission.clone(),
            returned: i64::from(result.returned),
            pin: state.pin,
            address,
            read_copy: state.copy_capture.as_deref().cloned(),
            socket: match state.completion.as_ref().unwrap().socket.as_ref() {
                Some(observed) => observed.checked(state.completion.as_ref().unwrap())?,
                None => None,
            },
        })
    }
    pub(crate) async fn retire_interrupted_read(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        publication
            .engine
            .lock()
            .unwrap()
            .validate_original_read_interrupted(owner, admission)
            .map_err(std::io::Error::other)?;
        // Existing Driver cancellation, exact command ACK and pin-close worker
        // must all finish. This wait holds neither metadata nor table exclusion.
        let state = self
            .wait_original(owner, admission, publication, true)
            .await?;
        if !state.canceled
            || state.completion.is_some()
            || state.selection.is_some()
            || state.terminal.is_some()
        {
            return Err(std::io::Error::other(
                "unentered Read retirement changed cancellation custody",
            ));
        }
        self.retire_original_connect(owner, admission, publication)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-3464): Review this sole-root semantic boundary and
    // the unchanged unpublished quota versus retained historical evidence.
    // https://github.com/rrnewton/hermit/pull/3464
    pub(crate) async fn checkpoint_completed_original_history(
        &self,
        owner: NetworkStreamOwner,
        publication: &NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        let Ok(root) = self.foreground_root(owner) else { return Ok(()); };
        if !root.is_sole_initial_root(owner) {
            return Ok(());
        }
        // The caller has retired this actual original Call and has not replied
        // to its stopped guest. No engine/native lock crosses this async wait.
        let mut journal = self.shared.fd_journal.lock().await;
        let metadata = root.metadata()?;
        let local = metadata.lock().unwrap();
        let engine = publication.engine.lock().unwrap();
        engine.validate_fd_metadata(owner, root.files(), &metadata, &local)
            .map_err(std::io::Error::other)?;
        let native = self.shared.native_streams.lock().unwrap();
        if root.is_sole_initial_root(owner)
            && engine.fd_history_checkpoint_ready()
            && local.pending_network_installations().is_empty()
            && native.settled().is_ok()
        {
            journal.checkpoint_after_semantic_retirement()?;
        }
        Ok(())
    }

    pub(crate) fn retire_original_connect(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publication: &NativeCaptureRecovery,
    ) -> std::io::Result<()> {
        if !self
            .shared
            .native_streams
            .lock()
            .unwrap()
            .original_closed(owner, admission.call)?
        {
            return Err(std::io::Error::other(
                "original consumer cannot retire a live pin",
            ));
        }
        publication.retire_original_lifetime(owner, admission, false)?;
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .finish_release(owner, admission.call)?;
        publication.changed.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
mod close_tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;
    use crate::network_replay::NetworkStreamOwner;
    use crate::network_runtime::accepted_provider_ffi as ffi;
    use crate::tool_local::FileMetadata;

    fn fixture(
        occupied: bool,
    ) -> (
        Arc<RuntimeShared>,
        NativeCaptureRecovery,
        NetworkStreamOwner,
        NetworkStreamOwner,
        Admission,
        Arc<Mutex<FileMetadata>>,
    ) {
        let (mut engine, metadata, owner, peer, admission) =
            crate::tool_local::original_close_tests::fixture(occupied);
        engine.original_connect_invoked(owner, &admission).unwrap();
        let publication = NativeCaptureRecovery::new(
            Arc::new(Mutex::new(engine)),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let shared = Arc::new(RuntimeShared {
            guard: Mutex::default(),
            endpoint: None,
            controller: Mutex::default(),
            driver: Mutex::default(),
            transport_terminal_deadline: Mutex::default(),
            incarnation: [91; 16],
            copy_wire: None,
            physical: Mutex::default(),
            accepted: Mutex::default(),
            listeners: Mutex::default(),
            creations: tokio::sync::Mutex::default(),
            fd_journal: tokio::sync::Mutex::default(),
            native_streams: Mutex::default(),
            native_workers: Mutex::default(),
            native_terminal_failure: Mutex::default(),
            record_receive_fixture: Mutex::default(),
        });
        shared
            .native_streams
            .lock()
            .unwrap()
            .capture_original(
                owner,
                admission.clone(),
                None,
                tokio::runtime::Handle::current(),
                publication.clone(),
            )
            .unwrap();
        (
            shared,
            publication,
            owner,
            peer,
            admission,
            Arc::new(Mutex::new(metadata)),
        )
    }
    fn selection(
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> accepted_provider::OriginalSelection {
        ffi::OriginalSelection {
            command: 17,
            call: admission.call.native_command_call(),
            owner_mm: owner.mm.generation(),
            provider: 5,
            task: 61,
            task_start: 99,
            table: 5,
            file: if admission.arguments.binding.is_some() {
                7
            } else {
                0
            },
            requested_fd: 7,
            ready: 1,
            ..Default::default()
        }
        .into()
    }
    fn completed(
        owner: NetworkStreamOwner,
        admission: &Admission,
        raw: i64,
    ) -> accepted_provider::OriginalEffect {
        let selected = selection(owner, admission);
        accepted_provider::OriginalEffect {
            socket: None,
            read_copy: None,
            send: None,
            blocking_send: None,
            command: ffi::CommandResult {
                command: 17,
                operation: 9,
                phase: 1,
                returned: raw as i32,
                task: 61,
                start_boottime: 99,
                identity: ffi::Identity {
                    provider: 5,
                    ..Default::default()
                },
                ..Default::default()
            }
            .into(),
            original: accepted_provider::OriginalResult {
                selection: selected,
                returned: raw as i32,
                complete: 1,
                ..ffi::OriginalResult::default().into()
            },
        }
    }
    #[tokio::test]
    async fn close_driver_binds_exact_metadata_before_early_publication_without_waiting_for_return()
    {
        let (shared, publication, owner, peer, admission, metadata) = fixture(true);
        let selected = selection(owner, &admission);
        let old = admission.arguments.binding.unwrap();
        assert!(
            shared
                .native_streams
                .lock()
                .unwrap()
                .bind_original_close_metadata(owner, &admission, metadata.clone())
                .is_err()
        );
        shared
            .native_streams
            .lock()
            .unwrap()
            .original_prepared(owner, &admission, 1, 17, 2)
            .unwrap();
        assert!(
            shared
                .publish_original_close_selection(owner, &admission, &publication, &selected)
                .is_err()
        );
        assert_eq!(metadata.lock().unwrap().descriptor_binding(7).unwrap(), old);
        shared
            .native_streams
            .lock()
            .unwrap()
            .bind_original_close_metadata(owner, &admission, metadata.clone())
            .unwrap();
        shared
            .native_streams
            .lock()
            .unwrap()
            .bind_original_close_metadata(owner, &admission, metadata.clone())
            .unwrap();
        assert_eq!(Arc::strong_count(&metadata), 2);
        let other = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(
            owner.thread,
        )));
        assert!(
            shared
                .native_streams
                .lock()
                .unwrap()
                .bind_original_close_metadata(owner, &admission, other)
                .is_err()
        );
        assert!(
            publication
                .engine
                .lock()
                .unwrap()
                .acquire_fd_publication(peer, admission.arguments.files)
                .is_err()
        );
        shared
            .native_streams
            .lock()
            .unwrap()
            .original_selected(owner, &admission, selected.clone())
            .unwrap();
        shared
            .publish_original_close_selection(owner, &admission, &publication, &selected)
            .unwrap();
        assert_eq!(
            metadata.lock().unwrap().descriptor_binding(7),
            Err(reverie::Errno::EBADF)
        );
        assert_eq!(
            publication
                .engine
                .lock()
                .unwrap()
                .original_connect_result(owner, &admission)
                .unwrap(),
            None
        );
        let permit = publication
            .engine
            .lock()
            .unwrap()
            .acquire_fd_publication(peer, admission.arguments.files)
            .unwrap()
            .permit;
        publication
            .engine
            .lock()
            .unwrap()
            .release_empty_fd_publication(peer, permit)
            .unwrap();
        assert!(
            shared
                .native_streams
                .lock()
                .unwrap()
                .original_reference(owner, admission.call)
                .unwrap()
                .is_none()
        );
        assert!(
            shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)
                .unwrap()
                .completion
                .is_none()
        );
        let raw = -i64::from(libc::EINTR);
        shared
            .native_streams
            .lock()
            .unwrap()
            .original_completed(owner, &admission, completed(owner, &admission, raw), raw)
            .unwrap();
        assert_eq!(
            shared
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)
                .unwrap()
                .completion
                .as_ref()
                .unwrap()
                .command
                .returned,
            -libc::EINTR
        );
        // Provider completion alone still does not invent the backend Returned observation.
        assert_eq!(
            publication
                .engine
                .lock()
                .unwrap()
                .original_connect_result(owner, &admission)
                .unwrap(),
            None
        );
    }
    #[tokio::test]
    async fn close_call_refuses_physical_pin_and_wrong_completion_without_losing_selection() {
        for occupied in [false, true] {
            for variant in 0..7 {
                let (shared, publication, owner, _, admission, metadata) = fixture(occupied);
                let mut isolated = native_peer::Calls::default();
                let pin: std::os::fd::OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
                assert!(
                    isolated
                        .capture_original(
                            owner,
                            admission.clone(),
                            Some(pin),
                            tokio::runtime::Handle::current(),
                            publication
                        )
                        .is_err()
                );
                assert!(isolated.originals().is_empty());
                let raw = if occupied {
                    -i64::from(libc::EINTR)
                } else {
                    -i64::from(libc::EBADF)
                };
                let selected = selection(owner, &admission);
                let mut calls = shared.native_streams.lock().unwrap();
                calls
                    .original_prepared(owner, &admission, 1, 17, 2)
                    .unwrap();
                calls
                    .bind_original_close_metadata(owner, &admission, metadata)
                    .unwrap();
                calls
                    .original_selected(owner, &admission, selected.clone())
                    .unwrap();
                let mut wrong = completed(owner, &admission, raw);
                match variant {
                    0 => wrong.command.operation = 7,
                    1 => wrong.original.selection.file ^= 1,
                    2 => wrong.original.complete = 0,
                    3 => wrong.original.copy_entered = 1,
                    4 => wrong.original.selection.fdput_flags = 1,
                    5 => {
                        wrong.command.returned = 1;
                        wrong.original.returned = 1;
                    }
                    6 => wrong.original.returned = 0,
                    _ => unreachable!(),
                }
                assert!(
                    calls
                        .original_completed(owner, &admission, wrong, raw)
                        .is_err(),
                    "occupied={occupied} variant={variant}"
                );
                let retained = calls.original(owner, admission.call).unwrap();
                assert_eq!(retained.selection, Some(selected));
                assert!(retained.completion.is_none());
                calls
                    .original_completed(owner, &admission, completed(owner, &admission, raw), raw)
                    .unwrap();
                assert!(
                    calls
                        .original_completed(
                            owner,
                            &admission,
                            completed(owner, &admission, raw),
                            raw
                        )
                        .is_err()
                );
            }
        }
    }
}

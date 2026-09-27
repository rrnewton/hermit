//! Typed native epoll_ctl observations from the existing Original command.
//! This module supplies no semantic registration or readiness authority. The
//! same Call must join both selected files to reserved logical lifetimes before
//! activation; current-slot rereads cannot supply that historical join.
use std::io;

use super::accepted_provider::OriginalEffect;
use super::accepted_provider::OriginalResult;
use super::accepted_provider::OriginalSelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Request {
    pub command: u64,
    pub call: u64,
    pub owner_mm: u64,
    pub provider: u64,
    pub epfd: i32,
    pub operation: i32,
    pub target_fd: i32,
    pub event_address: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Selection {
    /// Actual native control flow did not reach this lookup, not an empty FD.
    NotReached,
    Empty {
        cut: u64,
    },
    File {
        file: u64,
        fdput_flags: u64,
        cut: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    pub identity: OriginalSelection,
    /// Existing physical journal frontier at actual original syscall entry.
    pub before: u64,
    /// Twelve bytes Linux copied before either fdget; DEL supplies None.
    pub event: Option<[u8; 12]>,
    pub primary: Selection,
    pub target: Selection,
}
/// Private, non-deserializable proof of the actual Call's retained physical
/// history. It carries no live file reference and grants no current-slot use.
#[derive(Debug, Clone)]
pub(crate) struct HistoricalPair {
    owner: super::original_installation::Owner,
    capture: Capture,
    origins: [Option<super::accepted_provider::FdEvent>; 2],
}
impl HistoricalPair {
    pub(crate) fn capture(&self) -> &Capture {
        &self.capture
    }
    pub(crate) fn matches_metadata(
        &self,
        owner: crate::network_replay::NetworkStreamOwner,
        files: crate::types::FilesId,
        actual: &std::sync::Weak<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    ) -> bool {
        self.owner.owner == owner
            && self.owner.files == files
            && actual.ptr_eq(&std::sync::Arc::downgrade(&self.owner.metadata))
    }
    pub(super) fn from_history(
        owner: &super::original_installation::Owner,
        request: &Request,
        raw: &OriginalResult,
        history: &super::fd_journal::History,
    ) -> io::Result<Self> {
        let capture = validate_selection(request, raw)?;
        history.selection_prefix(capture.through())?;
        let s = &capture.identity;
        require(
            owner.owner.mm.generation() == request.owner_mm
                && (owner.provider, owner.task, owner.start, owner.table)
                    == (s.provider, s.task, s.task_start, s.table),
        )?;
        let mut origins = [None, None];
        for (index, selected) in [capture.primary, capture.target].iter().enumerate() {
            if let Selection::File { file, cut, .. } = *selected {
                let fd = if index == 0 {
                    request.epfd
                } else {
                    request.target_fd
                };
                origins[index] =
                    Some(history.unique_selection_origin(cut, owner.table, fd, file)?);
            }
        }
        // Keep both origin rows with the same Call until logical retirement.
        Ok(Self {
            owner: owner.clone(),
            capture,
            origins,
        })
    }
    pub(crate) fn origin_sequence(&self, index: usize) -> Option<u64> {
        self.origins
            .get(index)
            .and_then(|row| row.as_ref())
            .map(|row| row.sequence)
    }
}
impl Capture {
    pub(super) fn through(&self) -> u64 {
        [self.primary, self.target]
            .iter()
            .fold(self.before, |cut, selected| {
                cut.max(match selected {
                    Selection::NotReached => 0,
                    Selection::Empty { cut } | Selection::File { cut, .. } => *cut,
                })
            })
    }
}
fn require(ok: bool) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other("invalid original epoll_ctl receipt"))
    }
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn i32_at(bytes: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
}
/// Decode only after exact transport retention. No field is synthesized from
/// errno or a current numeric FD. A no-body native copy error is distinguished
/// from the two positively observed fdget-empty states.
pub(crate) fn validate_selection(request: &Request, raw: &OriginalResult) -> io::Result<Capture> {
    let s = &raw.selection;
    require(
        request.command != 0
            && request.call != 0
            && request.provider != 0
            && s.command == request.command
            && s.call == request.call
            && s.owner_mm == request.owner_mm
            && s.provider == request.provider
            && s.task != 0
            && s.task_start != 0
            && s.table != 0
            && s.ready == 1
            && s.requested_fd == request.epfd
            && s.address_length == request.operation
            && s.user_address == request.event_address
            && s.original_count == u64::from(request.target_fd as u32)
            && s.fdput_flags <= 1
            && (s.file != 0 || s.fdput_flags == 0)
            && raw.address.len() == 128
            && raw.problem == 0
            && raw.reserved == 0
            && raw.complete <= 1
            && (raw.complete == 1 || raw.returned == 0)
            && raw.copy_entered == 0
            && raw.copy_returned == 0
            && raw.copy_remaining == 0
            && raw.audit_entered == 0
            && raw.audit_returned == 0
            && raw.audit_result == 0
            && raw.security_entered == 0
            && raw.security_returned == 0
            && raw.security_result == 0,
    )?;
    let b = &raw.address;
    let entered = u64_at(b, 0);
    let body = u64_at(b, 8);
    let primary = u64_at(b, 16);
    let target = u64_at(b, 24);
    let target_file = u64_at(b, 32);
    let target_flags = u64_at(b, 40);
    let before = u64_at(b, 48);
    let primary_cut = u64_at(b, 56);
    let target_cut = u64_at(b, 64);
    let image_policy = u64_at(b, 72);
    let body_returned = u64_at(b, 96);
    let body_result = i32_at(b, 104);
    require(
        entered == 1
            && body <= 1
            && primary <= 1
            && target <= 1
            && target_flags <= 1
            && (target_file != 0 || target_flags == 0)
            && body_returned <= 1
            && (body_returned == 1 && body == 1 && (-4095..=0).contains(&body_result)
                || body_returned == 0 && body_result == 0)
            && (raw.complete == 0
                || (-4095..=0).contains(&raw.returned)
                    && (body == 0 || body_returned == 1 && body_result == raw.returned))
            && image_policy == 1
            && b[92..96].iter().all(|byte| *byte == 0)
            && b[108..].iter().all(|byte| *byte == 0),
    )?;
    let event: [u8; 12] = b[80..92].try_into().unwrap();
    if body == 0 {
        require(
            request.operation != libc::EPOLL_CTL_DEL
                && raw.complete == 1
                && raw.returned == -libc::EFAULT
                && primary == 0
                && target == 0
                && s.file == 0
                && target_file == 0
                && target_flags == 0
                && primary_cut == 0
                && target_cut == 0
                && event == [0; 12]
                && u64_at(b, 96) == 0
                && i32_at(b, 104) == 0,
        )?;
        return Ok(Capture {
            identity: s.clone(),
            before,
            event: None,
            primary: Selection::NotReached,
            target: Selection::NotReached,
        });
    }
    require(
        primary == 1
            && primary_cut >= before
            && (request.operation != libc::EPOLL_CTL_DEL || event == [0; 12]),
    )?;
    let (primary, target) = if s.file == 0 {
        require(target == 0 && target_file == 0 && target_flags == 0 && target_cut == 0)?;
        (Selection::Empty { cut: primary_cut }, Selection::NotReached)
    } else {
        require(target == 1 && target_cut >= primary_cut)?;
        (
            Selection::File {
                file: s.file,
                fdput_flags: s.fdput_flags,
                cut: primary_cut,
            },
            if target_file == 0 {
                Selection::Empty { cut: target_cut }
            } else {
                Selection::File {
                    file: target_file,
                    fdput_flags: target_flags,
                    cut: target_cut,
                }
            },
        )
    };
    Ok(Capture {
        identity: s.clone(),
        before,
        event: (request.operation != libc::EPOLL_CTL_DEL).then_some(event),
        primary,
        target,
    })
}
/// Linux has already executed the original ADD/MOD/DEL, including all error
/// ordering. This validates the result's provenance; it does not redo control.
pub(crate) fn validate_completion(
    request: &Request,
    effect: &OriginalEffect,
) -> io::Result<(Capture, i64)> {
    let raw = &effect.original;
    let capture = validate_selection(request, raw)?;
    let c = &effect.command;
    require(
        c.command == request.command
            && c.operation == 20
            && c.phase == 1
            && c.task == capture.identity.task
            && c.start_boottime == capture.identity.task_start
            && c.identity.provider == request.provider
            && c.identity.object == 0
            && c.identity.namespace == 0
            && c.creation == 0
            && c.cookie == 0
            && c.reserved == 0
            && c.original_count == u64::from(request.target_fd as u32)
            && c.returned == raw.returned
            && (-4095..=0).contains(&raw.returned)
            && raw.complete == 1
            && raw.copy_entered == 0
            && raw.copy_returned == 0
            && raw.copy_remaining == 0
            && raw.audit_entered == 0
            && raw.audit_returned == 0
            && raw.audit_result == 0
            && raw.security_entered == 0
            && raw.security_returned == 0
            && raw.security_result == 0
            && c.state
                == super::accepted_provider::RawState::from(
                    super::accepted_provider_ffi::RawState::default(),
                )
            && effect.socket.is_none()
            && effect.read_copy.is_none(),
    )?;
    if capture.primary != Selection::NotReached {
        require(
            u64_at(&raw.address, 96) == 1
                && i32_at(&raw.address, 104) == raw.returned
                && (matches!(capture.primary, Selection::File { .. })
                    && matches!(capture.target, Selection::File { .. })
                    || raw.returned == -libc::EBADF),
        )?;
    }
    Ok((capture, i64::from(raw.returned)))
}

impl super::RuntimeShared {
    /// Same Driver, controller, journal and native-worker registry as every
    /// Original Call. No scheduler, metadata or engine lock spans this wait.
    pub(super) fn progress_original_epoll_ctl(
        self: &std::sync::Arc<Self>,
        controller: &super::accepted_controller::Controller,
        owner: crate::network_replay::NetworkStreamOwner,
        state: &mut super::native_peer::OriginalConnect,
    ) -> io::Result<bool> {
        use super::accepted_provider::Reply;
        let admission = state.admission.clone();
        if state.selection.is_some() {
            return Ok(true);
        }
        if state.control_selection.is_none() {
            let request = state
                .selection_request
                .ok_or_else(|| io::Error::other("epoll lacks selection request"))?;
            let Some(reply) = controller.retained_response(request)? else {
                return Ok(false);
            };
            let Reply::OriginalControlSelection(observed) = reply else {
                return Err(io::Error::other(
                    "epoll pair was downgraded to single selection",
                ));
            };
            if observed.status.returned != 0 || observed.status.errno.is_some() {
                return Err(io::Error::other("epoll native pair remains unresolved"));
            }
            self.native_streams
                .lock()
                .unwrap()
                .original_control_selected(owner, &admission, observed.raw.clone())?;
            state.control_selection = Some(observed.raw);
        }
        if !state.control_history_started {
            let bound = self
                .native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .installation_owner
                .clone()
                .ok_or_else(|| io::Error::other("epoll lost bound physical owner"))?;
            let raw = state.control_selection.clone().unwrap();
            let request = Request {
                command: raw.selection.command,
                call: admission.call.native_command_call(),
                owner_mm: owner.mm.generation(),
                provider: bound.provider,
                epfd: admission.arguments.fd,
                operation: admission.arguments.length,
                target_fd: admission.arguments.original_count as u32 as i32,
                event_address: admission.arguments.address,
            };
            let through = validate_selection(&request, &raw)?.through();
            // Keep this exact existing controller owner alive through the
            // native worker. Cloning &Controller would retain only a borrow.
            let retained_controller = self
                .controller
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| io::Error::other("epoll history lacks retained controller"))?
                .map_err(io::Error::other)?;
            if !std::ptr::eq(retained_controller.as_ref(), controller) {
                return Err(io::Error::other(
                    "epoll history changed original controller owner",
                ));
            }
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .control_history_started = true;
            state.control_history_started = true;
            let shared = self.clone();
            let controller = retained_controller;
            let executor = state.executor.clone();
            let publication = state.publication.clone();
            let call = admission.call;
            let work = move || {
                let observed = executor.block_on(async {
                    let mut journal = shared.fd_journal.lock().await;
                    if through != 0 {
                        journal.through(&controller, owner, through).await?;
                    }
                    HistoricalPair::from_history(&bound, &request, &raw, journal.history())
                });
                let result = observed
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| io::Error::other(error.to_string()));
                shared
                    .native_streams
                    .lock()
                    .unwrap()
                    .original(owner, call)?
                    .control_history = Some(observed.map_err(|error| error.to_string()));
                publication.changed.notify_waiters();
                result
            };
            if let Err(error) = self.start_native_worker(state.executor.clone(), work) {
                self.native_streams
                    .lock()
                    .unwrap()
                    .original(owner, admission.call)?
                    .control_history = Some(Err(error.to_string()));
                return Err(error);
            }
        }
        state.control_history = self
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .control_history
            .clone();
        let Some(history) = &state.control_history else {
            return Ok(false);
        };
        let history = history
            .as_ref()
            .map_err(|error| io::Error::other(error.clone()))?;
        let ready = state
            .publication
            .engine
            .lock()
            .unwrap()
            .original_epoll_ctl_selected(owner, &admission, history)
            .map_err(io::Error::other)?;
        if !ready {
            return Ok(false);
        }
        let selected = history.capture().identity.clone();
        self.native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .selection = Some(selected.clone());
        state.selection = Some(selected);
        state.publication.changed.notify_waiters();
        Ok(true)
    }
}

/// Explicit component inputs traverse the production parser/history issuer;
/// they are not an assertion that a native callback executed.
#[cfg(test)]
pub(crate) fn controlled_history_fixture(
    owner: crate::network_replay::NetworkStreamOwner,
    metadata: std::sync::Arc<std::sync::Mutex<crate::tool_local::FileMetadata>>,
    admission: &crate::network_replay::original_connect::Admission,
    selected: [u64; 2],
) -> io::Result<(HistoricalPair, OriginalResult)> {
    use super::accepted_provider_ffi as ffi;
    let mut history = super::fd_journal::History::default();
    for (sequence, kind, file, dependency) in [
        (1, 1, 31, 0),
        (2, 2, 31, 1),
        (3, 3, 31, 0),
        (4, 1, 37, 0),
        (5, 2, 37, 4),
        (6, 3, 37, 0),
    ] {
        history.retain(
            ffi::FdStatus {
                next_event: 6,
                next_file: 37,
                next_table: 5,
                ..Default::default()
            }
            .into(),
            ffi::FdEvent {
                sequence,
                kind,
                task: 61,
                task_start: 99,
                table: 5,
                file,
                fd: admission.arguments.fd,
                dependency,
                complete: 1,
                ..Default::default()
            }
            .into(),
        )?;
    }
    let bound = super::original_installation::Owner {
        owner,
        metadata,
        files: admission.arguments.files,
        provider: 5,
        task: 61,
        start: 99,
        table: 5,
    };
    let request = Request {
        command: 17,
        call: admission.call.native_command_call(),
        owner_mm: owner.mm.generation(),
        provider: 5,
        epfd: admission.arguments.fd,
        operation: admission.arguments.length,
        target_fd: admission.arguments.original_count as u32 as i32,
        event_address: admission.arguments.address,
    };
    let mut raw: OriginalResult = ffi::OriginalResult::default().into();
    raw.selection = ffi::OriginalSelection {
        command: 17,
        call: request.call,
        owner_mm: request.owner_mm,
        provider: 5,
        task: 61,
        task_start: 99,
        table: 5,
        file: selected[0],
        requested_fd: request.epfd,
        address_length: request.operation,
        user_address: request.event_address,
        original_count: u64::from(request.target_fd as u32),
        ready: 1,
        ..Default::default()
    }
    .into();
    for (at, value) in [
        (0, 1),
        (8, 1),
        (16, 1),
        (24, 1),
        (32, selected[1]),
        (48, 2),
        (56, 3),
        (64, 6),
        (72, 1),
        (96, 1),
    ] {
        raw.address[at..at + 8].copy_from_slice(&u64::to_ne_bytes(value));
    }
    raw.complete = 1;
    Ok((
        HistoricalPair::from_history(&bound, &request, &raw, &history)?,
        raw,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn receipt() -> (Request, OriginalEffect) {
        let request = Request {
            command: 7,
            call: 11,
            owner_mm: 13,
            provider: 17,
            epfd: 3,
            operation: libc::EPOLL_CTL_ADD,
            target_fd: 4,
            event_address: 0x1000,
        };
        let mut effect: OriginalEffect =
            super::super::accepted_provider_ffi::OriginalEffect::default().into();
        let s = &mut effect.original.selection;
        *s = OriginalSelection {
            command: 7,
            call: 11,
            owner_mm: 13,
            provider: 17,
            task: 19,
            task_start: 23,
            table: 29,
            file: 31,
            fdput_flags: 1,
            ready: 1,
            requested_fd: 3,
            address_length: libc::EPOLL_CTL_ADD,
            original_count: 4,
            user_address: 0x1000,
        };
        for (at, value) in [
            (0, 1),
            (8, 1),
            (16, 1),
            (24, 1),
            (32, 37),
            (40, 1),
            (48, 5),
            (56, 6),
            (64, 9),
            (72, 1),
            (96, 1),
        ] {
            effect.original.address[at..at + 8].copy_from_slice(&u64::to_ne_bytes(value));
        }
        effect.original.address[80..84].copy_from_slice(&(libc::EPOLLIN as u32).to_ne_bytes());
        effect.original.address[84..92].copy_from_slice(&0xfeed_beef_9876_4321u64.to_ne_bytes());
        effect.original.complete = 1;
        let c = &mut effect.command;
        c.command = 7;
        c.operation = 20;
        c.phase = 1;
        c.task = 19;
        c.start_boottime = 23;
        c.identity.provider = 17;
        c.original_count = 4;
        (request, effect)
    }
    fn set(raw: &mut OriginalResult, at: usize, value: u64) {
        raw.address[at..at + 8].copy_from_slice(&value.to_ne_bytes());
    }
    #[tokio::test]
    async fn original_epoll_call_keeps_paired_raw_identity_and_rejects_single_selection() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use chrono::TimeZone;

        use crate::network_replay::NetworkReplayEngine;
        use crate::network_replay::NetworkStreamCallId;
        use crate::network_replay::NetworkStreamOwner;
        use crate::network_replay::original_connect::Admission;
        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        use crate::types::DetTid;
        use crate::types::FilesId;
        use crate::types::MmId;
        let thread = DetTid::from_raw(61);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let admission = Admission {
            call: NetworkStreamCallId::controlled_fixture(11),
            arguments: Arguments {
                kind: Kind::EpollCtl,
                operation: crate::resources::ExternalOpId::new(thread, 10),
                files: FilesId::initial(thread),
                binding: None,
                fd: 7,
                address: 0x2000,
                length: libc::EPOLL_CTL_ADD,
                original_count: 7,
            },
        };
        let metadata = Arc::new(Mutex::new(
            crate::tool_local::FileMetadata::empty_network_fixture(thread),
        ));
        let (history, raw) =
            controlled_history_fixture(owner, metadata.clone(), &admission, [31, 37]).unwrap();
        let publication = super::super::NativeCaptureRecovery::new(
            Arc::new(Mutex::new(NetworkReplayEngine::record(
                chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
            ))),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        );
        let mut calls = super::super::native_peer::Calls::default();
        calls
            .capture_original(
                owner,
                admission.clone(),
                None,
                tokio::runtime::Handle::current(),
                publication,
            )
            .unwrap();
        calls
            .original_prepared(owner, &admission, 1, 17, 2)
            .unwrap();
        let bound = super::super::original_installation::Owner {
            owner,
            metadata,
            files: admission.arguments.files,
            provider: 5,
            task: 61,
            start: 99,
            table: 5,
        };
        calls
            .bind_original_installation_owner(owner, &admission, bound)
            .unwrap();
        assert!(
            calls
                .original_selected(owner, &admission, raw.selection.clone())
                .is_err()
        );
        calls
            .original_control_selected(owner, &admission, raw.clone())
            .unwrap();
        assert!(
            calls
                .original(owner, admission.call)
                .unwrap()
                .selection
                .is_none()
        );
        for case in 0..7 {
            let mut wrong = raw.clone();
            match case {
                0 => wrong.selection.provider += 1,
                1 => wrong.selection.task += 1,
                2 => wrong.selection.task_start += 1,
                3 => wrong.selection.table += 1,
                4 => wrong.selection.owner_mm += 1,
                5 => wrong.selection.original_count += 1,
                6 => wrong.address[80] ^= 1,
                _ => unreachable!(),
            }
            assert!(
                calls
                    .original_control_selected(owner, &admission, wrong)
                    .is_err()
            );
            assert_eq!(
                calls
                    .original(owner, admission.call)
                    .unwrap()
                    .control_selection,
                Some(raw.clone())
            );
        }
        let mut effect: OriginalEffect =
            super::super::accepted_provider_ffi::OriginalEffect::default().into();
        effect.original = raw.clone();
        effect.command.command = 17;
        effect.command.operation = 20;
        effect.command.phase = 1;
        effect.command.task = 61;
        effect.command.start_boottime = 99;
        effect.command.identity.provider = 5;
        effect.command.original_count = 7;
        assert!(
            calls
                .original_completed(owner, &admission, effect.clone(), 0)
                .is_err()
        );
        // Explicit component premise: the production engine's paired join has
        // completed, retaining its exact private certificate in this same Call.
        let state = calls.original(owner, admission.call).unwrap();
        state.control_history = Some(Ok(history));
        state.selection = Some(raw.selection.clone());
        for case in 0..3 {
            let mut wrong = effect.clone();
            match case {
                0 => wrong.original.address[80] ^= 1,
                1 => wrong.original.selection.original_count += 1,
                2 => wrong.original.returned = -libc::EINVAL,
                _ => unreachable!(),
            }
            assert!(
                calls
                    .original_completed(owner, &admission, wrong, 0)
                    .is_err()
            );
            assert!(
                calls
                    .original(owner, admission.call)
                    .unwrap()
                    .completion
                    .is_none()
            );
        }
        calls
            .original_completed(owner, &admission, effect.clone(), 0)
            .unwrap();
        assert_eq!(
            calls.original(owner, admission.call).unwrap().completion,
            Some(effect)
        );
    }
    #[test]
    fn original_ctl_selection_preserves_scalar_envelope_and_early_body_return() {
        let (request, effect) = receipt();
        let mut early = effect.original.clone();
        early.complete = 0;
        assert!(validate_selection(&request, &early).is_ok());
        early.returned = -libc::EINVAL;
        assert!(validate_selection(&request, &early).is_err());
        early.returned = 0;
        early.address[104..108].copy_from_slice(&(-libc::EINVAL).to_ne_bytes());
        assert!(validate_selection(&request, &early).is_ok());
        early.complete = 1;
        assert!(validate_selection(&request, &early).is_err());
        early.returned = -libc::EINVAL;
        assert!(validate_selection(&request, &early).is_ok());
        for case in 0..11 {
            let mut bad = effect.original.clone();
            match case {
                0 => bad.complete = 2,
                1 => bad.copy_entered = 1,
                2 => bad.copy_returned = 1,
                3 => bad.copy_remaining = 1,
                4 => bad.audit_entered = 1,
                5 => bad.audit_returned = 1,
                6 => bad.audit_result = -libc::EFAULT,
                7 => bad.security_entered = 1,
                8 => bad.security_returned = 1,
                9 => bad.security_result = -libc::EACCES,
                10 => set(&mut bad, 96, 2),
                _ => unreachable!(),
            }
            assert!(validate_selection(&request, &bad).is_err(), "case {case}");
        }
    }
    #[test]
    fn original_ctl_keeps_both_actual_selections_and_opaque_data() {
        let (request, effect) = receipt();
        let (capture, result) = validate_completion(&request, &effect).unwrap();
        assert_eq!(result, 0);
        assert_eq!(
            capture.primary,
            Selection::File {
                file: 31,
                fdput_flags: 1,
                cut: 6
            }
        );
        assert_eq!(
            capture.target,
            Selection::File {
                file: 37,
                fdput_flags: 1,
                cut: 9
            }
        );
        assert_eq!(
            &capture.event.unwrap()[4..],
            &0xfeed_beef_9876_4321u64.to_ne_bytes()
        );
        for changed in [
            Request {
                command: 8,
                ..request
            },
            Request {
                target_fd: 3,
                ..request
            },
            Request {
                event_address: 0x1001,
                ..request
            },
            Request {
                operation: libc::EPOLL_CTL_MOD,
                ..request
            },
        ] {
            assert!(validate_completion(&changed, &effect).is_err());
        }
    }
    #[test]
    fn original_ctl_distinguishes_primary_empty_target_empty_and_unreached() {
        let (request, mut effect) = receipt();
        effect.command.returned = -libc::EBADF;
        effect.original.returned = -libc::EBADF;
        effect.original.address[104..108].copy_from_slice(&(-libc::EBADF).to_ne_bytes());
        set(&mut effect.original, 32, 0);
        set(&mut effect.original, 40, 0);
        assert_eq!(
            validate_completion(&request, &effect).unwrap().0.target,
            Selection::Empty { cut: 9 }
        );
        effect.original.selection.file = 0;
        effect.original.selection.fdput_flags = 0;
        assert!(validate_completion(&request, &effect).is_err()); // No second lookup after actual primary-empty.
        set(&mut effect.original, 24, 0);
        set(&mut effect.original, 64, 0);
        let capture = validate_completion(&request, &effect).unwrap().0;
        assert_eq!(capture.primary, Selection::Empty { cut: 6 });
        assert_eq!(capture.target, Selection::NotReached);
        effect.command.returned = 0;
        effect.original.returned = 0;
        effect.original.address[104..108].fill(0);
        assert!(validate_completion(&request, &effect).is_err());
    }
    #[test]
    fn original_ctl_copy_fault_requires_actual_entry_and_complete_return() {
        let (request, mut effect) = receipt();
        effect.original.selection.file = 0;
        effect.original.selection.fdput_flags = 0;
        effect.original.address.fill(0);
        set(&mut effect.original, 0, 1);
        set(&mut effect.original, 48, 5);
        set(&mut effect.original, 72, 1);
        effect.command.returned = -libc::EFAULT;
        effect.original.returned = -libc::EFAULT;
        let capture = validate_completion(&request, &effect).unwrap().0;
        assert_eq!(capture.primary, Selection::NotReached);
        assert_eq!(capture.target, Selection::NotReached);
        effect.original.complete = 0;
        assert!(validate_selection(&request, &effect.original).is_err());
        effect.original.complete = 1;
        set(&mut effect.original, 0, 0);
        assert!(validate_completion(&request, &effect).is_err());
        set(&mut effect.original, 0, 1);
        effect.original.selection.address_length = libc::EPOLL_CTL_DEL;
        assert!(
            validate_completion(
                &Request {
                    operation: libc::EPOLL_CTL_DEL,
                    ..request
                },
                &effect
            )
            .is_err()
        );
    }
    #[test]
    fn original_ctl_refuses_missing_target_changed_journal_and_del_copy() {
        let (request, effect) = receipt();
        for (at, value) in [(24, 0), (56, 4), (64, 5), (72, 0), (96, 0), (112, 1)] {
            let mut bad = effect.clone();
            set(&mut bad.original, at, value);
            assert!(validate_completion(&request, &bad).is_err(), "offset {at}");
        }
        let mut del = effect;
        del.original.selection.address_length = libc::EPOLL_CTL_DEL;
        let request = Request {
            operation: libc::EPOLL_CTL_DEL,
            event_address: u64::MAX,
            ..request
        };
        del.original.selection.user_address = request.event_address;
        assert!(validate_completion(&request, &del).is_err());
        del.original.address[80..92].fill(0);
        assert_eq!(validate_completion(&request, &del).unwrap().0.event, None);
    }
}

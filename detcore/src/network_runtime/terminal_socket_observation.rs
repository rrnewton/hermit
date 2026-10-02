//! Recover an original Socket's held observation after its backend owner exits.
//! The existing Call, controller Outbox and service Inbox retain every effect.
//! No replacement original syscall or original-command ACK is performed here.
use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;

use super::RuntimeShared;
use super::accepted_controller::Effect;
use super::accepted_provider::OriginalEffect;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::installation_observation::Checked;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Kind;

pub(super) fn validate_request(
    owner: NetworkStreamOwner,
    call: u64,
    effect: &OriginalEffect,
) -> io::Result<()> {
    let original = &effect.original;
    let selected = &original.selection;
    let command = &effect.command;
    if call == 0
        || selected.call != call
        || selected.owner_mm != owner.mm.generation()
        || selected.command == 0
        || selected.command != command.command
        || selected.provider == 0
        || selected.provider != command.identity.provider
        || selected.task == 0
        || selected.task != command.task
        || selected.task_start == 0
        || selected.task_start != command.start_boottime
        || selected.table == 0
        || selected.ready != 1
        || command.operation != Kind::Socket.provider_operation()
        || command.phase != 1
        || command.original_count != 0
        || command.reserved != 0
        || command.returned != original.returned
        || effect.socket.is_some()
        || super::original_installation::socket_result(original, i64::from(original.returned))?
            .is_none()
    {
        return Err(io::Error::other(
            "terminal Socket observation changed completed original identity",
        ));
    }
    Ok(())
}

impl RuntimeShared {
    pub(super) async fn observe_terminal_socket(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        admission: &Admission,
        publisher: NetworkStreamOwner,
        effect: &OriginalEffect,
    ) -> io::Result<Option<Checked>> {
        validate_request(owner, admission.call.native_command_call(), effect)?;
        let controller = self
            .controller
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("terminal Socket lost provider controller"))?
            .map_err(io::Error::other)?;
        // Check authority before acquiring a new PIDFD alias. A resumed request
        // borrows its retained rights; it never recaptures a successor or file.
        let state = self
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .clone();
        if state.admission != *admission
            || admission.arguments.kind != Kind::Socket
            || !state.retired
            || !state.close_queued
            || state.completed_allocator_effect()? != *effect
        {
            return Err(io::Error::other(
                "terminal Socket changed exact original Call",
            ));
        }
        if state.socket_observation_retired {
            return state
                .socket_observation
                .as_ref()
                .ok_or_else(|| io::Error::other("retired terminal Socket lost raw capture"))?
                .checked(effect);
        }
        let request = if let Some(request) = state.socket_observation_request {
            request
        } else {
            let bound = state
                .installation_owner
                .as_ref()
                .ok_or_else(|| io::Error::other("terminal Socket lacks Prepared metadata"))?;
            let metadata = bound.metadata.lock().unwrap();
            state
                .publication
                .engine
                .lock()
                .unwrap()
                .validate_fd_metadata(publisher, bound.files, &bound.metadata, &metadata)
                .map_err(io::Error::other)?;
            let physical = self.physical.lock().unwrap();
            let (provider, _, _, table) = physical.installation_identity(publisher)?;
            if (provider, table) != (bound.provider, bound.table) {
                return Err(io::Error::other(
                    "terminal Socket successor changed physical table",
                ));
            }
            let task = physical.get(publisher)?;
            let request = controller.prepare(
                Effect::ObserveTerminalSocket(admission.call),
                owner,
                &Request::ObserveTerminalSocket {
                    call: admission.call.native_command_call(),
                    effect: Box::new(effect.clone()),
                },
                || Ok(vec![task.as_fd().try_clone_to_owned()?]),
            )?;
            // Synchronous registration precedes the first await. A cancelled
            // recovery future cannot repeat the physical observation.
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .socket_observation_request = Some(request);
            request
        };
        let capture = if let Some(capture) = state.socket_observation {
            capture
        } else {
            let Reply::TerminalSocketObservation { call, capture } =
                controller.response(request).await?
            else {
                return Err(io::Error::other("terminal Socket changed response kind"));
            };
            if call != admission.call.native_command_call() {
                return Err(io::Error::other("terminal Socket response changed Call"));
            }
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .socket_observation = Some(capture.clone());
            capture
        };
        let checked = capture.checked(effect)?;
        let retirement = if let Some(request) = state.socket_observation_retirement_request {
            request
        } else {
            let retired = controller.prepare(
                Effect::RetireTerminalSocketObservation(admission.call),
                owner,
                &Request::RetireTerminalSocketObservation {
                    call: admission.call.native_command_call(),
                    observed: request,
                },
                || Ok(vec![]),
            )?;
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .socket_observation_retirement_request = Some(retired);
            retired
        };
        if !matches!(controller.response(retirement).await?, Reply::Retired) {
            return Err(io::Error::other(
                "terminal Socket retirement changed acknowledgement",
            ));
        }
        controller.retire_terminal_socket_observation(
            owner,
            admission.call,
            request,
            retirement,
        )?;
        self.native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .socket_observation_retired = true;
        Ok(checked)
    }
}

#[cfg(test)]
pub(super) fn fixture() -> (
    NetworkStreamOwner,
    OriginalEffect,
    super::installation_observation::Capture,
) {
    let thread = crate::types::DetTid::from_raw(41);
    let owner = NetworkStreamOwner {
        thread,
        mm: crate::types::MmId::initial(thread),
    };
    let (mut effect, capture) = super::installation_observation::fixture();
    let selected = &mut effect.original.selection;
    selected.call = 19;
    selected.owner_mm = owner.mm.generation();
    selected.command = effect.command.command;
    selected.provider = effect.command.identity.provider;
    selected.task = effect.command.task;
    selected.task_start = effect.command.start_boottime;
    selected.table = 29;
    selected.ready = 1;
    effect.original.complete = 1;
    effect.original.address[..8].copy_from_slice(&1u64.to_ne_bytes());
    effect.original.address[8..16].copy_from_slice(&2u64.to_ne_bytes());
    effect.original.address[16..20].copy_from_slice(&17i32.to_ne_bytes());
    (owner, effect, capture)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_socket_requires_complete_original_installation_identity() {
        let (owner, effect, capture) = fixture();
        validate_request(owner, 19, &effect).unwrap();
        for changed in 0..15 {
            let mut wrong = effect.clone();
            match changed {
                0 => wrong.original.selection.call += 1,
                1 => wrong.original.selection.owner_mm += 1,
                2 => wrong.original.selection.command += 1,
                3 => wrong.original.selection.provider += 1,
                4 => wrong.original.selection.task += 1,
                5 => wrong.original.selection.task_start += 1,
                6 => wrong.original.selection.table = 0,
                7 => wrong.original.selection.ready = 0,
                8 => wrong.command.operation = 14,
                9 => wrong.command.phase = 2,
                10 => wrong.command.returned = -libc::EBADF,
                11 => wrong.original.complete = 0,
                12 => wrong.original.problem = 1,
                13 => wrong.original.address[8..16].copy_from_slice(&1u64.to_ne_bytes()),
                14 => wrong.socket = Some(capture.clone()),
                _ => unreachable!(),
            }
            assert!(
                validate_request(owner, 19, &wrong).is_err(),
                "changed {changed}"
            );
        }
    }
}

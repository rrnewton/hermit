// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3174): paired native selection/lifetime join.
//! Paired original fdgets belong to the existing Call and lifetime reservation.
//! Proof-only binding history never pins a Linux file or excludes a table.
use super::*;
#[path = "epoll_ctl/foreground.rs"]
mod foreground;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use crate::network_runtime::original_epoll_ctl::Capture;
use crate::network_runtime::original_epoll_ctl::HistoricalPair;
use crate::network_runtime::original_epoll_ctl::Selection;
use crate::network_runtime::original_installation::FileIdentity;
use crate::tool_local::FileMetadata;

#[derive(Debug, Clone)]
pub(super) struct Control {
    pub(super) ticket: lifetime::SelectionTicket,
    metadata: Option<Weak<Mutex<FileMetadata>>>,
    bindings: Vec<(FdSlotBinding, FileIdentity)>,
    selected: Option<Capture>,
    foreground: Option<foreground::ForegroundAdmission>,
}
impl Control {
    pub(super) fn new(ticket: lifetime::SelectionTicket) -> Self {
        Self {
            ticket,
            metadata: None,
            bindings: Vec::new(),
            selected: None,
            foreground: None,
        }
    }
    fn note(
        &mut self,
        binding: FdSlotBinding,
        identity: FileIdentity,
    ) -> Result<(), NetworkReplayError> {
        if let Some((_, old)) = self.bindings.iter().find(|(old, _)| *old == binding) {
            return if *old == identity {
                Ok(())
            } else {
                Err(protocol(
                    "epoll history changed an authenticated native binding",
                ))
            };
        }
        if self.bindings.len() >= 128 {
            return Err(protocol("epoll native binding history capacity exhausted"));
        }
        self.bindings.push((binding, identity));
        Ok(())
    }
}
impl NetworkReplayEngine {
    pub(crate) fn begin_original_epoll_ctl(
        &mut self,
        owner: NetworkStreamOwner,
        arguments: Arguments,
    ) -> Result<Admission, NetworkReplayError> {
        if !self.fd_table_capability()
            || arguments.kind != Kind::EpollCtl
            || arguments.binding.is_some()
        {
            return Err(protocol(
                "original epoll control requires its exact paired reservation",
            ));
        }
        self.begin_original_call_with_mutation(
            owner,
            arguments,
            OriginalResultSource::Native,
            None,
            None,
        )
    }

    /// Actual Prepared callback, metadata -> engine. A deserialized argument
    /// cannot supply either this Arc or the private FileIdentity annotations.
    pub(crate) fn original_epoll_ctl_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
    ) -> Result<(), NetworkReplayError> {
        self.validate_fd_metadata(owner, admission.arguments.files, actual, local)?;
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || original.arguments.kind != Kind::EpollCtl
            || original.command.is_none()
            || original.pin.is_some()
            || original.final_wait
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
        {
            return Err(protocol(
                "epoll Prepared changed original no-exclusion custody",
            ));
        }
        let control = original
            .epoll_control
            .as_ref()
            .ok_or_else(|| protocol("epoll Prepared lost its reservation"))?;
        let weak = Arc::downgrade(actual);
        if control
            .metadata
            .as_ref()
            .is_some_and(|prior| !prior.ptr_eq(&weak))
        {
            return Err(protocol("epoll Prepared changed retained metadata object"));
        }
        let mut next = control.clone();
        for binding in self
            .lifetime
            .original_selection_candidates(control.ticket)
            .map_err(|error| protocol(&error.to_string()))?
        {
            if let Some(identity) = local.native_binding_identity(*binding) {
                next.note(*binding, identity)?;
            }
        }
        next.metadata = Some(weak);
        self.stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap()
            .epoll_control = Some(next);
        Ok(())
    }

    /// Existing publishers call this with private physical identity before
    /// removing metadata or pruning a receipt. Prepare every affected Call's
    /// bounded update before changing any of them; no partial history prefix.
    pub(in crate::network_replay) fn note_epoll_native_binding(
        &mut self,
        binding: FdSlotBinding,
        identity: FileIdentity,
    ) -> Result<(), NetworkReplayError> {
        let mut changes = Vec::new();
        for (call, state) in &self.stream_calls {
            let Some(original) = &state.original else {
                continue;
            };
            let Some(control) = &original.epoll_control else {
                continue;
            };
            if control.selected.is_some()
                || original.arguments.files != binding.slot.files
                || ![
                    original.arguments.fd,
                    original.arguments.original_count as u32 as i32,
                ]
                .contains(&binding.slot.fd)
            {
                continue;
            }
            let mut next = control.clone();
            next.note(binding, identity)?;
            changes.push((*call, next));
        }
        for (call, next) in changes {
            self.stream_calls
                .get_mut(&call)
                .unwrap()
                .original
                .as_mut()
                .unwrap()
                .epoll_control = Some(next);
        }
        Ok(())
    }

    pub(crate) fn note_epoll_published_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
        binding: FdSlotBinding,
    ) -> Result<(), NetworkReplayError> {
        self.validate_fd_metadata(owner, binding.slot.files, actual, local)?;
        if let Some(identity) = local.native_binding_identity(binding) {
            self.note_epoll_native_binding(binding, identity)?;
        }
        Ok(())
    }

    /// False means a real allocator/alias has not yet published its semantic
    /// generation. Its native result stays in the same Call; no errno is made.
    pub(crate) fn original_epoll_ctl_selected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        history: &HistoricalPair,
    ) -> Result<bool, NetworkReplayError> {
        let capture = history.capture();
        let s = &capture.identity;
        let (state, original) = self.original_connect_state(owner, admission.call)?;
        if original.arguments != admission.arguments
            || admission.arguments.kind != Kind::EpollCtl
            || original.command != Some(s.command)
            || original.uninvoked
            || s.call != admission.call.native_command_call()
            || s.owner_mm != owner.mm.generation()
            || s.requested_fd != admission.arguments.fd
            || s.user_address != admission.arguments.address
            || s.address_length != admission.arguments.length
            || s.original_count != admission.arguments.original_count
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
        {
            return Err(protocol("paired epoll selection changed its original Call"));
        }
        let control = original
            .epoll_control
            .as_ref()
            .ok_or_else(|| protocol("epoll reservation missing"))?;
        if !control.metadata.as_ref().is_some_and(|metadata| {
            history.matches_metadata(owner, admission.arguments.files, metadata)
        }) {
            return Err(protocol(
                "epoll pair changed actual Prepared metadata/table owner",
            ));
        }
        if let Some(previous) = &control.selected {
            return if previous == capture {
                Ok(true)
            } else {
                Err(protocol("epoll selected pair changed"))
            };
        }
        let candidates = self
            .lifetime
            .original_selection_candidates(control.ticket)
            .map_err(|error| protocol(&error.to_string()))?;
        let mut selected = [None, None];
        for (index, observation) in [capture.primary, capture.target].iter().enumerate() {
            let Selection::File { file, .. } = *observation else {
                continue;
            };
            if history.origin_sequence(index).is_none() {
                return Err(protocol(
                    "epoll native selection lost retained historical origin",
                ));
            }
            let fd = if index == 0 {
                admission.arguments.fd
            } else {
                admission.arguments.original_count as u32 as i32
            };
            let mut matching = Vec::new();
            for binding in candidates.iter().filter(|binding| binding.slot.fd == fd) {
                let Some((_, identity)) =
                    control.bindings.iter().find(|(known, _)| known == binding)
                else {
                    return Ok(false);
                };
                if identity.matches(s.provider, file) {
                    matching.push(*binding);
                }
            }
            if matching.is_empty() {
                return Ok(false);
            }
            // A post-fdget counter is not a Linux slot-generation linearization
            // point. Identical-file reinstallation cannot choose an arbitrary
            // generation, even though its epoll key has the same fd+file pair.
            if matching.len() != 1 {
                return Err(protocol(
                    "epoll selection has ambiguous same-file slot generations",
                ));
            }
            selected[index] = Some(matching[0]);
        }
        if let Some(foreground) = &control.foreground {
            // An authenticated pre-fdget failure (for example exact input
            // EFAULT) has no selected files. Canonical native validation owns
            // that path; absence inferred from a raw errno cannot enter here.
            let before_fdgets =
                capture.primary == Selection::NotReached && capture.target == Selection::NotReached;
            if !before_fdgets && selected != foreground.bindings.map(Some) {
                return Err(protocol(
                    "foreground ctl selected a different actual native pair",
                ));
            }
        }
        let ticket = control.ticket;
        let retired = self
            .lifetime
            .resolve_original_selection(ticket, selected)
            .map_err(|error| protocol(&error.to_string()))?;
        self.retire_lifetime_open_files(retired);
        let original = self
            .stream_calls
            .get_mut(&admission.call)
            .unwrap()
            .original
            .as_mut()
            .unwrap();
        original.epoll_control.as_mut().unwrap().selected = Some(capture.clone());
        original.selected = Some((s.provider, s.task, s.task_start, s.table, s.file));
        Ok(true)
    }
}

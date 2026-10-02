//! Logical custody while an original syscall has not reported its fdgets.
//! This does not acquire a native descriptor, lock a table, prove selection,
//! or keep a kernel file open. The existing Call must join each selected
//! generation to its authenticated physical journal cut before resolution.

use super::*;

// The physical journal already refuses more than 128 unresolved rows. Keep
// this Call's possible historical bindings bounded too; never evict one to
// admit another installation. This is not a larger native journal allowance.
const MAX_SELECTION_BINDINGS: usize = 128;

/// Reservation named by the existing Call's lifetime lease, not a new allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SelectionTicket {
    owner: TaskOwner,
    files: FilesId,
    lease: LeaseId,
    descriptors: [RawFd; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingSelection {
    ticket: SelectionTicket,
    candidates: Vec<FdSlotBinding>,
    selected: Option<[Option<FdSlotBinding>; 2]>,
    // Preserve unresolved facts after actual failed-run physical retirement.
    // This tombstone no longer owns an OFD, but still forbids a successful trace.
    terminal: Option<super::super::original_connect::SelectionTerminal>,
}

impl PendingSelection {
    fn observes(&self, files: FilesId, fd: RawFd) -> bool {
        self.selected.is_none()
            && self.terminal.is_none()
            && self.ticket.files == files
            && self.ticket.descriptors.contains(&fd)
    }
}

impl NetworkLifetime {
    /// Reserve possible logical selections before original uaccess. A missing
    /// slot and a negative fd are valid possibilities; neither fabricates EBADF.
    /// This leaves slots, task sharing, and every ordinary publication permit
    /// untouched, so a fault resolver can close and replace either descriptor.
    pub(crate) fn prepare_original_selection(
        &mut self,
        owner: TaskOwner,
        lease: LeaseId,
        descriptors: [RawFd; 2],
    ) -> Result<SelectionTicket, LifetimeError> {
        let files = self.task(owner)?.files;
        self.new_lease(owner, lease)?;
        if lease.kind != LeaseKind::StreamCall {
            return Err(LifetimeError::SelectionIdentity(lease));
        }
        let ticket = SelectionTicket {
            owner,
            files,
            lease,
            descriptors,
        };
        let mut candidates = Vec::new();
        for fd in descriptors {
            if let Some(binding) = self.binding_in_table(files, fd)
                && !candidates.contains(&binding)
            {
                candidates.push(binding);
            }
        }
        assert!(self.used_leases.insert(lease));
        assert!(
            self.pending_selections
                .insert(
                    lease,
                    PendingSelection {
                        ticket,
                        candidates,
                        selected: None,
                        terminal: None,
                    }
                )
                .is_none()
        );
        Ok(ticket)
    }

    fn pending_selection(
        &self,
        ticket: SelectionTicket,
    ) -> Result<&PendingSelection, LifetimeError> {
        self.pending_selections
            .get(&ticket.lease)
            .filter(|pending| pending.ticket == ticket)
            .ok_or(LifetimeError::SelectionIdentity(ticket.lease))
    }

    /// Return only retained semantic candidates. These are not physical
    /// selection receipts, and current numeric-slot lookup cannot replace the
    /// caller's native-file identity and exact journal-prefix validation.
    pub(crate) fn original_selection_candidates(
        &self,
        ticket: SelectionTicket,
    ) -> Result<&[FdSlotBinding], LifetimeError> {
        Ok(&self.pending_selection(ticket)?.candidates)
    }

    #[cfg(test)]
    pub(crate) fn original_selection_resolution(
        &self,
        ticket: SelectionTicket,
    ) -> Result<Option<[Option<FdSlotBinding>; 2]>, LifetimeError> {
        Ok(self.pending_selection(ticket)?.selected)
    }

    /// Narrow after BOTH independently authenticated lookup outcomes. The
    /// caller distinguishes an actual empty fdget from a lookup not reached;
    /// both have no selected OFD here. Selecting an old incarnation remains
    /// legal after close/reuse because the reservation prevented retirement.
    pub(crate) fn resolve_original_selection(
        &mut self,
        ticket: SelectionTicket,
        selected: [Option<FdSlotBinding>; 2],
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let pending = self.pending_selection(ticket)?;
        if pending.selected.is_some() || pending.terminal.is_some() {
            return Err(LifetimeError::SelectionIdentity(ticket.lease));
        }
        for (index, binding) in selected.iter().enumerate() {
            if let Some(binding) = binding
                && (binding.slot.files != ticket.files
                    || binding.slot.fd != ticket.descriptors[index]
                    || !pending.candidates.contains(binding)
                    || !self.live.contains_key(&binding.open_file)
                    || self.retired.contains(&binding.open_file))
            {
                return Err(LifetimeError::SelectionIdentity(ticket.lease));
            }
        }
        let pending = self.pending_selections.get_mut(&ticket.lease).unwrap();
        pending
            .candidates
            .retain(|binding| selected.contains(&Some(*binding)));
        pending.selected = Some(selected);
        Ok(self.collect_retired())
    }

    /// Settle only with the existing Call's actual completion/cancellation
    /// authority. Owner exit does not settle this reservation. Unknown effects
    /// and even an unresolved empty candidate set prevent run finalization.
    pub(crate) fn finish_original_selection(
        &mut self,
        ticket: SelectionTicket,
        resolution: TransportResolution,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let pending = self.pending_selection(ticket)?;
        if pending.terminal.is_some() {
            return Err(LifetimeError::SelectionIdentity(ticket.lease));
        }
        match resolution {
            TransportResolution::CompletedAndRecorded if pending.selected.is_some() => {}
            TransportResolution::CancellationAcknowledgedBeforeSubmission
                if pending.selected.is_none() => {}
            TransportResolution::UnknownEffects => {
                return Err(LifetimeError::UnresolvedTransport(ticket.lease));
            }
            _ => return Err(LifetimeError::SelectionIdentity(ticket.lease)),
        }
        self.pending_selections.remove(&ticket.lease);
        Ok(self.collect_retired())
    }

    /// Actual final-wait and complete native/provider retirement can release
    /// possible OFDs even when no lookup outcome was ever observed. Keep the
    /// same original facts as a failed-run tombstone; do not invent a result,
    /// turn unknown into empty, or permit finish() to accept this trace.
    pub(crate) fn retire_original_selection_after_terminal(
        &mut self,
        ticket: SelectionTicket,
        evidence: super::super::original_connect::SelectionTerminal,
    ) -> Result<BTreeSet<OpenFileId>, LifetimeError> {
        let pending = self.pending_selection(ticket)?;
        if pending.terminal.is_some() || !evidence.matches(ticket.owner, ticket.files, ticket.lease)
        {
            return Err(LifetimeError::SelectionIdentity(ticket.lease));
        }
        self.pending_selections
            .get_mut(&ticket.lease)
            .unwrap()
            .terminal = Some(evidence);
        Ok(self.collect_retired())
    }

    pub(super) fn check_selection_installation(
        &self,
        files: FilesId,
        fd: RawFd,
    ) -> Result<(), LifetimeError> {
        // The refusal names this installation, not the first HashMap entry.
        // Multiple full reservations cannot choose a host-dependent owner.
        if self.pending_selections.values().any(|pending| {
            pending.observes(files, fd) && pending.candidates.len() >= MAX_SELECTION_BINDINGS
        }) {
            return Err(LifetimeError::SelectionCapacity { files, fd });
        }
        Ok(())
    }

    pub(super) fn note_selection_installation(&mut self, binding: FdSlotBinding) {
        for pending in self.pending_selections.values_mut() {
            if pending.observes(binding.slot.files, binding.slot.fd) {
                assert!(pending.candidates.len() < MAX_SELECTION_BINDINGS);
                assert!(!pending.candidates.contains(&binding));
                pending.candidates.push(binding);
            }
        }
    }

    pub(super) fn selection_reservations(&self, object: OpenFileId) -> usize {
        self.pending_selections
            .values()
            .filter(|pending| {
                pending.terminal.is_none()
                    && pending
                        .candidates
                        .iter()
                        .any(|binding| binding.open_file == object)
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(tid: i32) -> TaskOwner {
        let tid = DetTid::from_raw(tid);
        TaskOwner {
            tid,
            mm: MmId::initial(tid),
        }
    }
    fn object(sequence: u64) -> OpenFileId {
        OpenFileId::new_socket(owner(10).tid, sequence)
    }
    fn slot(sequence: u64) -> NetworkSlot {
        NetworkSlot {
            open_file: object(sequence),
            cloexec: false,
        }
    }
    fn call(owner: TaskOwner, sequence: u64) -> LeaseId {
        LeaseId {
            operation: ExternalOpId::new(owner.tid, sequence),
            mm: owner.mm,
            kind: LeaseKind::StreamCall,
            ordinal: 0,
        }
    }
    fn setup() -> (NetworkLifetime, TaskOwner) {
        let owner = owner(10);
        let mut state = NetworkLifetime::default();
        state
            .register(owner, owner.tid, FilesId::initial(owner.tid))
            .unwrap();
        (state, owner)
    }

    #[test]
    fn unresolved_selection_keeps_only_possible_generations_through_shared_close_and_reuse() {
        let (mut state, owner) = setup();
        let peer = self::owner(11);
        state.open(owner, 3, slot(0)).unwrap();
        state.open(owner, 4, slot(1)).unwrap();
        let old_primary = state.descriptor_binding(owner, 3).unwrap();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [3, 4])
            .unwrap();
        state.share_table(owner, peer, peer.tid).unwrap();
        assert!(state.close(peer, 3, object(0)).unwrap().is_empty());
        state.open(peer, 3, slot(2)).unwrap();
        assert!(state.close(peer, 4, object(1)).unwrap().is_empty());
        state.open(peer, 4, slot(3)).unwrap();
        let new_target = state.descriptor_binding(peer, 4).unwrap();
        assert!(state.close(peer, 4, object(3)).unwrap().is_empty());
        assert_eq!(
            state.original_selection_candidates(ticket).unwrap().len(),
            4
        );
        // Explicit component input: the provider/journal join must prove these
        // two distinct native lookup cuts before the engine calls this method.
        assert_eq!(
            state
                .resolve_original_selection(ticket, [Some(old_primary), Some(new_target)])
                .unwrap(),
            BTreeSet::from([object(1)])
        );
        assert_eq!(
            state.counts(object(0)),
            OwnerCounts {
                selection_reservations: 1,
                ..OwnerCounts::default()
            }
        );
        assert_eq!(
            state.counts(object(3)),
            OwnerCounts {
                selection_reservations: 1,
                ..OwnerCounts::default()
            }
        );
        assert_eq!(
            state.descriptor_binding(peer, 3).unwrap().open_file,
            object(2)
        );
        assert_eq!(
            state
                .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                .unwrap(),
            BTreeSet::from([object(0), object(3)])
        );
        assert!(state.exit(owner).unwrap().is_empty());
        assert_eq!(state.exit(peer).unwrap(), BTreeSet::from([object(2)]));
        state.finish().unwrap();
    }

    #[test]
    fn two_lookups_of_the_same_number_can_select_different_historical_generations() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0)).unwrap();
        let first = state.descriptor_binding(owner, 3).unwrap();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [3, 3])
            .unwrap();
        assert!(state.close(owner, 3, object(0)).unwrap().is_empty());
        state.open(owner, 3, slot(1)).unwrap();
        let second = state.descriptor_binding(owner, 3).unwrap();
        assert!(state.close(owner, 3, object(1)).unwrap().is_empty());
        state.open(owner, 3, slot(2)).unwrap();
        assert_eq!(
            state.original_selection_candidates(ticket).unwrap().len(),
            3
        );
        assert!(
            state
                .resolve_original_selection(ticket, [Some(first), Some(second)])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            state
                .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                .unwrap(),
            BTreeSet::from([object(0), object(1)])
        );
        assert_eq!(
            state.descriptor_binding(owner, 3).unwrap().open_file,
            object(2)
        );
    }

    #[test]
    fn selection_identity_failures_and_repeated_resolution_preserve_all_custody() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0)).unwrap();
        let binding = state.descriptor_binding(owner, 3).unwrap();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [3, 4])
            .unwrap();
        for change in 0..7 {
            let before = state.clone();
            let mut changed = ticket;
            let mut selected = [Some(binding), None];
            match change {
                0 => changed.owner = self::owner(11),
                1 => changed.files = FilesId::initial(self::owner(11).tid),
                2 => changed.lease = call(owner, 2),
                3 => changed.descriptors = [4, 3],
                4 => selected[0].as_mut().unwrap().generation += 1,
                5 => selected[0].as_mut().unwrap().open_file = object(99),
                6 => selected = [None, Some(binding)],
                _ => unreachable!(),
            }
            assert!(
                state.resolve_original_selection(changed, selected).is_err(),
                "case {change}"
            );
            assert_eq!(state, before, "case {change}");
        }
        state
            .resolve_original_selection(ticket, [Some(binding), None])
            .unwrap();
        let before = state.clone();
        assert!(
            state
                .resolve_original_selection(ticket, [Some(binding), None])
                .is_err()
        );
        assert!(
            state
                .finish_original_selection(
                    ticket,
                    TransportResolution::CancellationAcknowledgedBeforeSubmission
                )
                .is_err()
        );
        assert_eq!(state, before);
        state
            .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
            .unwrap();
        let before = state.clone();
        assert!(
            state
                .prepare_original_selection(owner, call(owner, 1), [3, 4])
                .is_err()
        );
        assert!(
            state
                .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                .is_err()
        );
        assert_eq!(state, before);
    }

    #[test]
    fn owner_exit_and_empty_candidates_do_not_acknowledge_an_original_invocation() {
        for has_file in [false, true] {
            let (mut state, owner) = setup();
            if has_file {
                state.open(owner, 3, slot(0)).unwrap();
            }
            let ticket = state
                .prepare_original_selection(owner, call(owner, 1), [3, -1])
                .unwrap();
            assert!(state.exit(owner).unwrap().is_empty());
            assert_eq!(state.finish(), Err(LifetimeError::OutstandingOwners));
            let before = state.clone();
            assert!(
                state
                    .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                    .is_err()
            );
            assert!(
                state
                    .finish_original_selection(ticket, TransportResolution::CompletedEmulation)
                    .is_err()
            );
            assert!(
                state
                    .finish_original_selection(ticket, TransportResolution::UnknownEffects)
                    .is_err()
            );
            assert_eq!(state, before);
            // An authenticated no-fdget native completion can resolve both
            // roles after the initiating task is already gone.
            let retired = state
                .resolve_original_selection(ticket, [None, None])
                .unwrap();
            assert_eq!(
                retired,
                if has_file {
                    BTreeSet::from([object(0)])
                } else {
                    BTreeSet::new()
                }
            );
            assert_eq!(state.finish(), Err(LifetimeError::OutstandingOwners));
            assert!(
                state
                    .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                    .unwrap()
                    .is_empty()
            );
            state.finish().unwrap();
        }
        let (mut state, owner) = setup();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [-1, -2])
            .unwrap();
        state.exit(owner).unwrap();
        state
            .finish_original_selection(
                ticket,
                TransportResolution::CancellationAcknowledgedBeforeSubmission,
            )
            .unwrap();
        state.finish().unwrap();
    }

    #[test]
    fn selection_capacity_refuses_every_installation_route_without_partial_mutation() {
        let (mut state, owner) = setup();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [3, 4])
            .unwrap();
        for sequence in 0..MAX_SELECTION_BINDINGS as u64 {
            state.open(owner, 3, slot(sequence)).unwrap();
            assert!(state.close(owner, 3, object(sequence)).unwrap().is_empty());
        }
        assert_eq!(
            state.original_selection_candidates(ticket).unwrap().len(),
            MAX_SELECTION_BINDINGS
        );
        // Unrelated descriptors still progress when these two histories fill.
        state.open(owner, 9, slot(999)).unwrap();
        let source = state.descriptor_binding(owner, 9).unwrap();
        let transfer = LeaseId {
            kind: LeaseKind::Transfer,
            ..call(owner, 2)
        };
        state.retain_binding(owner, source, transfer).unwrap();
        let files = FilesId::initial(owner.tid);
        let next = state.tables[&files].last_slot_generation + 1;
        let installed = NetworkFdSlot {
            binding: FdSlotBinding {
                slot: FdSlot { files, fd: 3 },
                generation: next,
                open_file: object(1000),
            },
            cloexec: false,
        };
        let aliased = NetworkFdSlot {
            binding: FdSlotBinding {
                open_file: object(999),
                ..installed.binding
            },
            ..installed
        };
        let batch = SlotPublicationBatch {
            files,
            sequence: 1,
            previous_generation: next - 1,
            through_generation: next,
            entries: vec![SlotPublicationEntry {
                replacement: NetworkFdSlotReplacement {
                    files,
                    installation_generation: next,
                    before: None,
                    after: Some(installed),
                },
                source: SlotInstallationSource::Fresh,
            }],
        };
        let before = state.clone();
        for route in 0..6 {
            let rejected = match route {
                0 => state.open(owner, 3, slot(1000)).map(|_| ()),
                1 => state.duplicate(owner, 9, object(999), 3, false).map(|_| ()),
                2 => state
                    .install_transfer(owner, 3, object(999), transfer, false)
                    .map(|_| ()),
                3 => state
                    .publish_created_slot(owner, installed, None)
                    .map(|_| ()),
                4 => state
                    .publish_duplicated_slot(owner, source, aliased, None)
                    .map(|_| ()),
                5 => state.publish_installation_batch(owner, &batch).map(|_| ()),
                _ => unreachable!(),
            };
            assert_eq!(
                rejected,
                Err(LifetimeError::SelectionCapacity { files, fd: 3 }),
                "route {route}"
            );
            assert_eq!(state, before, "route {route}");
        }
        let retired = state
            .resolve_original_selection(ticket, [None, None])
            .unwrap();
        assert_eq!(
            retired,
            (0..MAX_SELECTION_BINDINGS as u64).map(object).collect()
        );
        state.open(owner, 3, slot(1000)).unwrap();
        assert_eq!(state.original_selection_candidates(ticket).unwrap(), &[]);
    }

    #[test]
    fn narrowing_stops_following_new_aliases_and_never_retains_an_unrelated_slot() {
        let (mut state, owner) = setup();
        state.open(owner, 3, slot(0)).unwrap();
        state.open(owner, 9, slot(1)).unwrap();
        let selected = state.descriptor_binding(owner, 3).unwrap();
        let ticket = state
            .prepare_original_selection(owner, call(owner, 1), [3, 4])
            .unwrap();
        assert_eq!(
            state.close(owner, 9, object(1)).unwrap(),
            BTreeSet::from([object(1)])
        );
        state
            .resolve_original_selection(ticket, [Some(selected), None])
            .unwrap();
        assert!(state.close(owner, 3, object(0)).unwrap().is_empty());
        for sequence in 2..MAX_SELECTION_BINDINGS as u64 + 4 {
            state.open(owner, 3, slot(sequence)).unwrap();
            assert_eq!(
                state.close(owner, 3, object(sequence)).unwrap(),
                BTreeSet::from([object(sequence)])
            );
        }
        assert_eq!(
            state.original_selection_candidates(ticket).unwrap(),
            &[selected]
        );
        assert_eq!(
            state
                .finish_original_selection(ticket, TransportResolution::CompletedAndRecorded)
                .unwrap(),
            BTreeSet::from([object(0)])
        );
    }
}

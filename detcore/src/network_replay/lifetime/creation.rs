//! Compact original birth authority inside the existing live-OFD ledger.
//! No serialized source, current slot lookup, ACK payload or initial census can
//! manufacture it. Once an alias/lineage/removal occurs it cannot be restored.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginalCreationKind {
    Socket,
    Epoll,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OriginalCreation {
    owner: TaskOwner,
    binding: FdSlotBinding,
    kind: OriginalCreationKind,
}
impl NetworkLifetime {
    // Called within the candidate's atomic, validated FreshOriginal entry.
    pub(super) fn remember_original_creation(
        &mut self,
        owner: TaskOwner,
        binding: FdSlotBinding,
        kind: OriginalCreationKind,
    ) {
        assert_eq!(
            self.binding_in_table(binding.slot.files, binding.slot.fd),
            Some(binding)
        );
        let live = self
            .live
            .get_mut(&binding.open_file)
            .expect("validated fresh live object");
        assert!(live.is_none());
        *live = Some(OriginalCreation {
            owner,
            binding,
            kind,
        });
    }
    pub(in crate::network_replay) fn has_original_creation(
        &self,
        owner: TaskOwner,
        binding: FdSlotBinding,
        kind: OriginalCreationKind,
    ) -> bool {
        self.validate_binding(owner, binding).is_ok()
            && self.live.get(&binding.open_file)
                == Some(&Some(OriginalCreation {
                    owner,
                    binding,
                    kind,
                }))
    }
    pub(super) fn revoke_original_creation(&mut self, object: OpenFileId) {
        if let Some(live) = self.live.get_mut(&object) {
            *live = None;
        }
    }
    pub(super) fn revoke_table_original_creations(&mut self, files: FilesId) {
        if let Some(table) = self.tables.get(&files) {
            for slot in table.slots.values() {
                if let Some(live) = self.live.get_mut(&slot.open_file) {
                    *live = None;
                }
            }
        }
    }
    pub(super) fn revoke_clone_original_creations(&mut self, ticket: CloneTicket) {
        if let Some(pending) = self.pending_clones.get(&ticket.operation) {
            for slot in pending.table.slots.values() {
                if let Some(live) = self.live.get_mut(&slot.open_file) {
                    *live = None;
                }
            }
        }
        self.revoke_table_original_creations(ticket.files);
    }
    pub(super) fn revoke_owner_original_creations(&mut self, owner: TaskOwner) {
        for live in self.live.values_mut() {
            if live.is_some_and(|creation| creation.owner == owner) {
                *live = None;
            }
        }
    }
    #[cfg(test)]
    pub(in crate::network_replay) fn clear_original_creation_for_test(
        &mut self,
        binding: FdSlotBinding,
    ) {
        assert_eq!(
            self.binding_in_table(binding.slot.files, binding.slot.fd),
            Some(binding)
        );
        self.revoke_original_creation(binding.open_file);
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
    fn object(n: u64) -> OpenFileId {
        OpenFileId::new_socket(owner(51).tid, n)
    }
    fn slot(who: TaskOwner, fd: i32, generation: u64, n: u64) -> NetworkFdSlot {
        NetworkFdSlot {
            binding: FdSlotBinding {
                slot: FdSlot {
                    files: FilesId::initial(who.tid),
                    fd,
                },
                generation,
                open_file: object(n),
            },
            cloexec: false,
        }
    }
    fn fixture(
        kind: OriginalCreationKind,
    ) -> (
        NetworkLifetime,
        TaskOwner,
        FdSlotBinding,
        SlotPublicationBatch,
    ) {
        let who = owner(51);
        let files = FilesId::initial(who.tid);
        let mut state = NetworkLifetime::default();
        state.register(who, who.tid, files).unwrap();
        let after = slot(who, 7, 1, 1);
        let batch = SlotPublicationBatch {
            files,
            sequence: 1,
            previous_generation: 0,
            through_generation: 1,
            entries: vec![SlotPublicationEntry {
                replacement: NetworkFdSlotReplacement {
                    files,
                    installation_generation: 1,
                    before: None,
                    after: Some(after),
                },
                source: SlotInstallationSource::FreshOriginal { owner: who, kind },
            }],
        };
        state.publish_installation_batch(who, &batch).unwrap();
        state.acknowledge_publication_batch(who, 1, 1).unwrap();
        (state, who, after.binding, batch)
    }
    #[test]
    fn original_creation_survives_ack_only_for_exact_birth_identity_and_kind() {
        for kind in [OriginalCreationKind::Socket, OriginalCreationKind::Epoll] {
            let (state, who, binding, _) = fixture(kind);
            assert_eq!(state.pending_publication_payloads_for_test(), 0);
            assert!(state.has_original_creation(who, binding, kind));
            let before = state.clone();
            assert!(!state.has_original_creation(owner(52), binding, kind));
            assert!(!state.has_original_creation(
                TaskOwner {
                    mm: who.mm.for_exec(who.tid),
                    ..who
                },
                binding,
                kind
            ));
            for changed in [
                FdSlotBinding {
                    generation: binding.generation + 1,
                    ..binding
                },
                FdSlotBinding {
                    slot: FdSlot {
                        fd: 8,
                        ..binding.slot
                    },
                    ..binding
                },
                FdSlotBinding {
                    slot: FdSlot {
                        files: FilesId::forked(owner(52).tid),
                        ..binding.slot
                    },
                    ..binding
                },
                FdSlotBinding {
                    open_file: object(2),
                    ..binding
                },
            ] {
                assert!(!state.has_original_creation(who, changed, kind));
            }
            let wrong = if kind == OriginalCreationKind::Socket {
                OriginalCreationKind::Epoll
            } else {
                OriginalCreationKind::Socket
            };
            assert!(!state.has_original_creation(who, binding, wrong));
            assert_eq!(
                state, before,
                "identity refusals must not consume live authority"
            );
        }
    }
    #[test]
    fn original_creation_is_not_issued_by_census_generic_open_or_fresh_publication() {
        for variant in 0..4 {
            let who = owner(51);
            let files = FilesId::initial(who.tid);
            let mut state = NetworkLifetime::default();
            let after = slot(who, 7, 1, 1);
            if variant == 0 {
                state
                    .register_census(who, who.tid, files, &[after], 1)
                    .unwrap();
            } else {
                state.register(who, who.tid, files).unwrap();
                match variant {
                    1 => state
                        .open(
                            who,
                            7,
                            NetworkSlot {
                                open_file: object(1),
                                cloexec: false,
                            },
                        )
                        .unwrap(),
                    2 => {
                        state.publish_created_slot(who, after, None).unwrap();
                    }
                    3 => {
                        let batch = SlotPublicationBatch {
                            files,
                            sequence: 1,
                            previous_generation: 0,
                            through_generation: 1,
                            entries: vec![SlotPublicationEntry {
                                replacement: NetworkFdSlotReplacement {
                                    files,
                                    installation_generation: 1,
                                    before: None,
                                    after: Some(after),
                                },
                                source: SlotInstallationSource::Fresh,
                            }],
                        };
                        state.publish_installation_batch(who, &batch).unwrap();
                        state.acknowledge_publication_batch(who, 1, 1).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
            assert!(
                !state.has_original_creation(who, after.binding, OriginalCreationKind::Socket),
                "case {variant}"
            );
            assert!(
                !state.has_original_creation(who, after.binding, OriginalCreationKind::Epoll),
                "case {variant}"
            );
        }
    }
    #[test]
    fn original_creation_alias_transfer_close_and_reuse_cannot_restore_authority() {
        for variant in 0..6 {
            let (mut state, who, birth, _) = fixture(OriginalCreationKind::Socket);
            // Actual same-fd dup2 and descriptor flags do not create aliases.
            state.duplicate(who, 7, birth.open_file, 7, true).unwrap();
            state.set_cloexec_binding(who, birth, false).unwrap();
            assert!(state.has_original_creation(who, birth, OriginalCreationKind::Socket));
            let alias = slot(who, 8, 2, 1);
            match variant {
                0 => {
                    state.duplicate(who, 7, birth.open_file, 8, false).unwrap();
                }
                1 => {
                    state
                        .publish_duplicated_slot(who, birth, alias, None)
                        .unwrap();
                }
                2 => {
                    state
                        .publish_installation_batch(
                            who,
                            &SlotPublicationBatch {
                                files: birth.slot.files,
                                sequence: 2,
                                previous_generation: 1,
                                through_generation: 2,
                                entries: vec![SlotPublicationEntry {
                                    replacement: NetworkFdSlotReplacement {
                                        files: birth.slot.files,
                                        installation_generation: 2,
                                        before: None,
                                        after: Some(alias),
                                    },
                                    source: SlotInstallationSource::Alias(birth),
                                }],
                            },
                        )
                        .unwrap();
                }
                3 => {
                    let lease = LeaseId {
                        operation: ExternalOpId::new(who.tid, 7),
                        mm: who.mm,
                        kind: LeaseKind::Transfer,
                        ordinal: 0,
                    };
                    state.retain_binding(who, birth, lease).unwrap();
                    state.release_lease(lease, birth.open_file).unwrap();
                }
                4 => {
                    let lease = LeaseId {
                        operation: ExternalOpId::new(who.tid, 8),
                        mm: who.mm,
                        kind: LeaseKind::Transport,
                        ordinal: 0,
                    };
                    state.retain_binding(who, birth, lease).unwrap();
                    state.close_binding(who, birth).unwrap();
                    assert!(
                        !state.is_retired(birth.open_file),
                        "ordinary custody keeps the object live, not its creation permission"
                    );
                    state
                        .open(
                            who,
                            7,
                            NetworkSlot {
                                open_file: object(2),
                                cloexec: false,
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        state
                            .acknowledge_transport(
                                lease,
                                birth.open_file,
                                TransportResolution::CompletedAndRecorded
                            )
                            .unwrap(),
                        BTreeSet::from([birth.open_file])
                    );
                }
                5 => {
                    assert_eq!(
                        state.close_binding(who, birth).unwrap(),
                        BTreeSet::from([birth.open_file])
                    );
                    state
                        .open(
                            who,
                            7,
                            NetworkSlot {
                                open_file: object(2),
                                cloexec: false,
                            },
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            if variant <= 2 {
                assert!(!state.has_original_creation(
                    who,
                    alias.binding,
                    OriginalCreationKind::Socket
                ));
                state.close(who, 8, birth.open_file).unwrap();
            }
            assert!(
                !state.has_original_creation(who, birth, OriginalCreationKind::Socket),
                "case {variant}"
            );
            let current = state.descriptor_binding(who, 7).unwrap();
            assert!(
                !state.has_original_creation(who, current, OriginalCreationKind::Socket),
                "case {variant}"
            );
        }
    }
    #[test]
    fn original_creation_lineage_is_revoked_after_actual_clone_share_copy_exec_or_exit() {
        for variant in 0..8 {
            let (mut state, who, birth, _) = fixture(OriginalCreationKind::Epoll);
            let child = owner(52);
            match variant {
                0 => {
                    state.share_table(who, child, child.tid).unwrap();
                    state.exit(child).unwrap();
                }
                1 => {
                    state
                        .copy_table(who, child, child.tid, FilesId::forked(child.tid))
                        .unwrap();
                    state.exit(child).unwrap();
                }
                2 | 3 => {
                    let ticket = CloneTicket {
                        owner: who,
                        files: birth.slot.files,
                        operation: ExternalOpId::new(who.tid, 8),
                    };
                    state.prepare_clone(ticket, variant == 2).unwrap();
                    state.commit_clone(ticket, child, child.tid).unwrap();
                    state.exit(child).unwrap();
                }
                4 => {
                    let mut allocator = crate::types::FilesIdAllocator::default();
                    let ticket = ExecFilesReceipt {
                        caller: who.tid,
                        process: who.tid,
                        mm: who.mm,
                        old_files: birth.slot.files,
                        new_files: allocator.allocate_exec(who.tid),
                    };
                    state.prepare_exec(ticket).unwrap();
                    let current = TaskOwner {
                        tid: who.tid,
                        mm: who.mm.for_exec(who.tid),
                    };
                    state
                        .commit_exec(
                            ticket,
                            &ExecReconnect {
                                caller: who.tid,
                                new_leader: who.tid,
                                detpid: who.tid,
                                pre_exec_mm: who.mm,
                                post_exec_mm: current.mm,
                                child_tid_addr: 0,
                                reconnect_priority: None,
                            },
                        )
                        .unwrap();
                    let inherited = state.descriptor_binding(current, 7).unwrap();
                    assert!(!state.has_original_creation(
                        current,
                        inherited,
                        OriginalCreationKind::Epoll
                    ));
                }
                5 => {
                    let lease = LeaseId {
                        operation: ExternalOpId::new(who.tid, 9),
                        mm: who.mm,
                        kind: LeaseKind::Transport,
                        ordinal: 0,
                    };
                    state.retain_binding(who, birth, lease).unwrap();
                    state.exit(who).unwrap();
                    assert!(!state.is_retired(birth.open_file));
                    assert_eq!(state.live[&birth.open_file], None);
                }
                6 | 7 => {
                    let ticket = CloneTicket {
                        owner: who,
                        files: birth.slot.files,
                        operation: ExternalOpId::new(who.tid, 10),
                    };
                    state.prepare_clone(ticket, true).unwrap();
                    if variant == 7 {
                        state.defer_clone_choice(ticket).unwrap();
                        state.resolve_clone_choice(ticket, false).unwrap();
                    }
                    state.retire_prestart_clone(ticket, child).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                !state.has_original_creation(who, birth, OriginalCreationKind::Epoll),
                "case {variant}"
            );
            assert_eq!(
                state.live.get(&birth.open_file),
                Some(&None),
                "surviving OFD is not new authority in case {variant}"
            );
        }
    }
    #[test]
    fn original_creation_late_bad_publication_preserves_entire_candidate_and_ack_fence() {
        let (mut state, who, birth, old) = fixture(OriginalCreationKind::Socket);
        let before = state.clone();
        assert!(
            state.publish_installation_batch(who, &old).is_err(),
            "ACK never restores a consumed full receipt"
        );
        assert_eq!(state, before);
        let alias = slot(who, 8, 2, 1);
        let collision = slot(who, 9, 3, 1);
        let bad = SlotPublicationBatch {
            files: birth.slot.files,
            sequence: 2,
            previous_generation: 1,
            through_generation: 3,
            entries: vec![
                SlotPublicationEntry {
                    replacement: NetworkFdSlotReplacement {
                        files: birth.slot.files,
                        installation_generation: 2,
                        before: None,
                        after: Some(alias),
                    },
                    source: SlotInstallationSource::Alias(birth),
                },
                SlotPublicationEntry {
                    replacement: NetworkFdSlotReplacement {
                        files: birth.slot.files,
                        installation_generation: 3,
                        before: None,
                        after: Some(collision),
                    },
                    source: SlotInstallationSource::FreshOriginal {
                        owner: who,
                        kind: OriginalCreationKind::Socket,
                    },
                },
            ],
        };
        assert!(state.publish_installation_batch(who, &bad).is_err());
        assert_eq!(
            state, before,
            "rejected alias prefix must not revoke or create authority"
        );
        assert!(state.has_original_creation(who, birth, OriginalCreationKind::Socket));
        let replacement = slot(who, 7, 2, 2);
        state
            .publish_installation_batch(
                who,
                &SlotPublicationBatch {
                    files: birth.slot.files,
                    sequence: 2,
                    previous_generation: 1,
                    through_generation: 2,
                    entries: vec![SlotPublicationEntry {
                        replacement: NetworkFdSlotReplacement {
                            files: birth.slot.files,
                            installation_generation: 2,
                            before: Some(NetworkFdSlot {
                                binding: birth,
                                cloexec: false,
                            }),
                            after: Some(replacement),
                        },
                        source: SlotInstallationSource::Fresh,
                    }],
                },
            )
            .unwrap();
        assert!(state.is_retired(birth.open_file));
        assert!(!state.has_original_creation(
            who,
            replacement.binding,
            OriginalCreationKind::Socket
        ));
    }
}
